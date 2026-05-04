import React from 'react';
import { Button, Col, Form, Row } from 'react-bootstrap';
import Subtitle from '@components/shared/titles/Subtitle';
import UploadDropzone from '@components/shared/UploadDropzone';
import { TagSelect } from '@components/tags/TagSelect';
import SelectInputArray from '@components/shared/selectable/SelectInputArray';
import { OverlayTipTop } from '@components/shared/overlay/tips';
import SelectPipelines from '../reactions/SelectPipelines';
import ProgressBarContainer from './ProgressBarContainer';
import TLPSelection from './TLPSelection';
import OriginForm from './OriginForm';
import UploadAlertBanner from './UploadAlertBanner';
import {
  DropzoneFile,
  FileUploadStatus,
  OriginState,
  ReactionSubmitResult,
  TagEntry,
  TLPSelection as TLPSelectionState,
} from './types';

interface UploadFormProps {
  uploadInProgress: boolean;
  filesArray: DropzoneFile[];
  setFilesArray: (files: DropzoneFile[]) => void;
  selectedGroups: string[];
  setSelectedGroups: (groups: string[]) => void;
  userGroups: string[];
  description: string;
  setDescription: (desc: string) => void;
  tags: TagEntry[];
  setTags: (tags: TagEntry[]) => void;
  selectedTLP: TLPSelectionState;
  onTLPChange: (selection: TLPSelectionState) => void;
  originState: OriginState;
  onOriginChange: (field: keyof OriginState, value: string) => void;
  reactionsList: any[];
  setReactionsList: (reactions: any[]) => void;
  userInfo: any;
  onUpload: () => void;
  uploadStatus: Record<string, FileUploadStatus>;
  uploadError: string[];
  setUploadError: (errors: string[]) => void;
  uploadSHA256: string[];
  runReactionsRes: ReactionSubmitResult[];
  onResetStatus: () => void;
}

const UploadForm: React.FC<UploadFormProps> = ({
  uploadInProgress,
  filesArray,
  setFilesArray,
  selectedGroups,
  setSelectedGroups,
  userGroups,
  description,
  setDescription,
  tags,
  setTags,
  selectedTLP,
  onTLPChange,
  originState,
  onOriginChange,
  reactionsList,
  setReactionsList,
  userInfo,
  onUpload,
  uploadStatus,
  uploadError,
  setUploadError,
  uploadSHA256,
  runReactionsRes,
  onResetStatus,
}) => {
  const disabledClass = uploadInProgress ? 'disabled ' : '';

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
        <Col className={disabledClass + 'upload-field'}>
          <UploadDropzone onChange={setFilesArray} onError={setUploadError} selectedFiles={filesArray} width="100%" />
          <br />
        </Col>
      </Row>
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
        <Col className={disabledClass + 'upload-field'}>
          <SelectInputArray
            isCreatable={false}
            options={userGroups}
            values={selectedGroups.sort()}
            onChange={(groups: string[]) => setSelectedGroups(groups)}
          />
        </Col>
      </Row>
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
        <Col className={disabledClass + 'upload-field'}>
          <Form.Control
            className="description-field"
            as="textarea"
            placeholder="Add Description"
            value={description}
            onChange={(e) => {
              setDescription(e.target.value);
              onResetStatus();
            }}
          />
        </Col>
      </Row>
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
        <Col className={disabledClass + 'upload-field'}>
          <TagSelect tags={tags} setTags={setTags} placeholderText="Add Tags" />
        </Col>
      </Row>
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
        <Col className={disabledClass + 'upload-field'}>
          <TLPSelection selectedTLP={selectedTLP} onTLPChange={onTLPChange} />
        </Col>
      </Row>
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
        <Col className={disabledClass + 'upload-field'}>
          <OriginForm originState={originState} onOriginChange={onOriginChange} onResetStatus={onResetStatus} />
        </Col>
      </Row>
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
        <Col className={disabledClass + 'upload-field'}>
          <SelectPipelines
            userInfo={userInfo}
            setReactionsList={setReactionsList}
            setError={setUploadError}
            currentSelections={reactionsList}
          />
        </Col>
      </Row>
      <Row className="mt-3">
        <Col className="upload-field-name" />
        <Col className="upload-field ms-4">
          <p>
            <sup>*</sup> This field is required.
          </p>
        </Col>
      </Row>
      <Row>
        <Col className="upload-field-name" />
        <Col className="upload-field ms-4">
          <p>
            <sup>T</sup> This field also creates tags when specified.
          </p>
        </Col>
      </Row>
      <Row className="d-flex justify-content-center">
        <Col className="upload-field-name"></Col>
        <Col className="upload-field">
          {uploadStatus && Object.entries(uploadStatus).length > 0 && (
            <Row className="upload-bar mt-3">
              {Object.entries(uploadStatus).map(([key, value]) => (
                <OverlayTipTop key={key} tip={value.msg}>
                  {key}
                  <ProgressBarContainer name={key} value={value.progress} error={uploadError.length > 0} />
                </OverlayTipTop>
              ))}
            </Row>
          )}
          {!uploadInProgress && (
            <>
              <Row className="upload_alerts">
                <Col className="upload-field">
                  <UploadAlertBanner uploadSHA256={uploadSHA256} uploadError={uploadError} runReactionsRes={runReactionsRes} />
                </Col>
              </Row>
              <Row className="d-flex justify-content-center upload-btn">
                <Col className="upload-field">
                  <center>
                    <Button className="ok-btn" onClick={onUpload}>
                      Upload
                    </Button>
                  </center>
                </Col>
              </Row>
            </>
          )}
        </Col>
      </Row>
    </>
  );
};

export default UploadForm;
