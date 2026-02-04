import React from 'react';
import { Col, Row } from 'react-bootstrap';
import { Subtitle, SelectableDictionary } from '@components';
import { useUploadForm } from './upload_context';

/**
 * Component for the tags section
 * Allows users to add key-value tag pairs to the uploaded file
 */
export const TagsSection: React.FC = () => {
  const { state, dispatch } = useUploadForm();

  return (
    <>
      <Row className="mb-4 alt-label">
        <Col className="upload-field-name"></Col>
        <Col className="upload-field-name-alt">
          <Subtitle>Tags</Subtitle>
        </Col>
      </Row>
      <Row className="mb-2">
        <Col className="upload-field-name">
          <Subtitle>Tags</Subtitle>
        </Col>
        <Col className={(state.uploadInProgress ? 'disabled ' : '') + 'upload-field'}>
          <SelectableDictionary
            entries={state.tags}
            setEntries={(tags: Array<{ key: string; value: string }>) => dispatch({ type: 'SET_TAGS', payload: tags })}
            keyPlaceholder="Add Tag Key"
            valuePlaceholder="Add Tag Value"
            disabled={state.uploadInProgress}
            keys={null}
            deleted={null}
            setDeleted={null}
            trim={false}
          />
        </Col>
      </Row>
    </>
  );
};
