import React from 'react';
import { Col, Row } from 'react-bootstrap';
import { Subtitle, SelectPipelines } from '@components';
import { useAuth } from '@utilities';
import { useUploadForm } from '../context';

/**
 * Component for the pipelines selection section
 * Allows users to select which pipelines to run on the uploaded file
 */
export const PipelinesSection: React.FC = () => {
  const { userInfo } = useAuth();
  const { state, dispatch } = useUploadForm();

  return (
    <>
      <Row className="mb-4 alt-label">
        <Col className="upload-field-name"></Col>
        <Col className="upload-field-name-alt">
          <Subtitle>Run Pipelines</Subtitle>
        </Col>
      </Row>
      <Row>
        <Col className="upload-field-name">
          <Subtitle>Run Pipelines</Subtitle>
        </Col>
        <Col className={(state.uploadInProgress ? 'disabled ' : '') + 'upload-field'}>
          <SelectPipelines
            userInfo={userInfo}
            setReactionsList={(reactions: any[]) => dispatch({ type: 'SET_REACTIONS_LIST', payload: reactions })}
            setError={(errors: string[]) => dispatch({ type: 'SET_UPLOAD_ERROR', payload: errors })}
            currentSelections={state.reactionsList}
          />
        </Col>
      </Row>
    </>
  );
};
