import React from 'react';
import { Col, Row } from 'react-bootstrap';
import { Subtitle, SelectInputArray } from '@components';
import { useAuth } from '@utilities';
import { useUploadForm } from '../context';

/**
 * Component for the groups selection section
 * Allows users to select which groups have access to the uploaded file
 */
export const GroupsSelectionSection: React.FC = () => {
  const { userInfo } = useAuth();
  const { state, dispatch } = useUploadForm();

  return (
    <>
      <Row className="mb-4 alt-label">
        <Col className="upload-field-name"></Col>
        <Col className="upload-field-name-alt">
          <Subtitle>
            Groups <sup>*</sup>
          </Subtitle>
        </Col>
      </Row>
      <Row className="mb-4">
        <Col className="upload-field-name">
          <Subtitle>
            Groups <sup>*</sup>
          </Subtitle>
        </Col>
        <Col className={(state.uploadInProgress ? 'disabled ' : '') + 'upload-field'}>
          <SelectInputArray
            isCreatable={false}
            options={userInfo?.groups ? userInfo.groups.sort() : []}
            values={state.groups.sort()}
            onChange={(groups: string[]) => dispatch({ type: 'SET_GROUPS', payload: groups })}
          />
        </Col>
      </Row>
    </>
  );
};
