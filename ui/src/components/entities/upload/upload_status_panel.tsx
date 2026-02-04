import React, { Fragment } from 'react';
import { Link } from 'react-router-dom';
import { Button, Card, Col, Row } from 'react-bootstrap';
import { FaChevronDown, FaChevronUp, FaRedo } from 'react-icons/fa';
import { OverlayTipTop, Subtitle } from '@components';
import { ProgressBarContainer } from './progress_bar_container';
import { useUploadForm } from './upload_context';

type UploadStatusPanelProps = {
  computeTotal: () => number;
  retryFileUpload: (fileName: string) => void;
  retryAllFileUploads: () => void;
  retrySubmitReaction: (status: any) => void;
  retryAllReactionSubmissions: () => void;
  cancelUpload: () => void;
};

/**
 * Component for displaying upload status panel with progress and results
 * Shows overall progress, file statuses, and reaction results
 */
export const UploadStatusPanel: React.FC<UploadStatusPanelProps> = ({
  computeTotal,
  retryFileUpload,
  retryAllFileUploads,
  retrySubmitReaction,
  retryAllReactionSubmissions,
  cancelUpload,
}) => {
  const { state, dispatch } = useUploadForm();

  const resetStatusMessages = () => {
    dispatch({ type: 'RESET_STATUS_MESSAGES' });
  };

  const setShowUploadStatus = (show: boolean) => {
    dispatch({ type: 'SET_SHOW_UPLOAD_STATUS', payload: show });
  };
  return (
    <Fragment>
      Total
      <Row className="upload-bar">
        <Col>
          <ProgressBarContainer name={'Total'} value={computeTotal()} error={state.uploadError.length} />
        </Col>
      </Row>
      {state.uploadInProgress && (
        <Row className="upload-bar">
          {Object.values(state.activeUploads).map((key: any) => (
            <OverlayTipTop key={key} tip={state.uploadStatus[key].msg}>
              {key}
              <ProgressBarContainer name={key} value={state.uploadStatus[key].progress} error={state.uploadStatus[key].fileFail} />
            </OverlayTipTop>
          ))}
        </Row>
      )}
      {!state.uploadInProgress && (
        <Card className="stats-container panel">
          <Card.Body>
            <div>{Object.keys(state.uploadStatus).length - Object.keys(state.uploadFailures).length} Files Uploaded Successfully</div>
            {Object.keys(state.uploadFailures).length > 0 && (
              <div>
                {Object.keys(state.uploadFailures).length} File Upload Failure(s)
                <Button
                  size={'xsm' as any}
                  variant="no-outline-secondary"
                  className="retry-button"
                  onClick={() => {
                    retryAllFileUploads();
                  }}
                >
                  {' '}
                  <FaRedo />
                </Button>
              </div>
            )}
            <div>{state.uploadReactionRes.length - state.uploadReactionFailures} Reaction(s) Submitted Successfully</div>
            {state.uploadReactionFailures > 0 && (
              <div>
                {state.uploadReactionFailures} Reaction Submission(s) Failed
                <Button
                  size={'xsm' as any}
                  variant="no-outline-secondary"
                  className="retry-button"
                  onClick={() => {
                    retryAllReactionSubmissions();
                  }}
                >
                  {' '}
                  <FaRedo />
                </Button>
              </div>
            )}
          </Card.Body>
        </Card>
      )}
      <Row className="mt-1">
        <Card className="panel">
          <Row>
            <Col className="status-dropdown" md={1} />
            <Col className="status-file" md={1}>
              <Subtitle>Filename</Subtitle>
            </Col>
            <Col className="status-msg" md={1}>
              <Subtitle>Status</Subtitle>
            </Col>
            <Col className="status-percent" md={1}>
              <Subtitle>Progress</Subtitle>
            </Col>
            <Col className="status-sha-head">
              <Subtitle>SHA256</Subtitle>
            </Col>
          </Row>
        </Card>
      </Row>
      <Row className="mt-1">
        {Object.entries(state.uploadStatus).map(([key, value]: [string, any]) => (
          <Fragment key={key}>
            <Card>
              <Row>
                <Col className="status-dropdown" md={1}>
                  <Button
                    size={'xsm' as any}
                    variant="no-outline-secondary"
                    onClick={() =>
                      dispatch({
                        type: 'UPDATE_UPLOAD_STATUS_DROPDOWN',
                        payload: { key, value: !state.uploadStatusDropdown[key] },
                      })
                    }
                  >
                    {state.uploadStatusDropdown[key] ? <FaChevronUp /> : <FaChevronDown />}
                  </Button>
                </Col>
                <Col className="status-file" md={1}>
                  {key}
                </Col>
                <Col className={'status-msg' + (value.fileFail | value.reactionFail ? ' status-error' : '')} md={1}>
                  {value.msg}
                </Col>
                <Col className="status-percent" md={1}>
                  {value.fileFail ? '0' : value.progress}%
                </Col>
                {value.sha256 && (
                  <Link to={`/file/${value.sha256}`} target="_blank" className="status-sha-link link-text-alt">
                    <Col className="status-sha">{value.sha256}</Col>
                  </Link>
                )}
                {!value.sha256 && !state.uploadInProgress && (
                  <Col className="status-sha">
                    <Button size={'xsm' as any} variant="no-outline-secondary" className="redo-btn" onClick={(e) => retryFileUpload(key)}>
                      <FaRedo />
                    </Button>
                  </Col>
                )}
              </Row>
            </Card>
            {state.uploadStatusDropdown[key] &&
              state.uploadReactions[key] &&
              (state.uploadReactions[key].length === 0 ? (
                <Row className="upload-content">
                  <b>No Reactions Submitted</b>
                </Row>
              ) : (
                <div className="reaction-uploads-card">
                  <Card className="panel">
                    <Row className="reaction-row mt-1">
                      <Col md={2}>
                        <Subtitle>Pipeline</Subtitle>
                      </Col>
                      <Col md={2}>
                        <Subtitle>Group</Subtitle>
                      </Col>
                      <Col md={3}>
                        <Subtitle>Error</Subtitle>
                      </Col>
                      <Col md={2}>
                        <Subtitle>ID</Subtitle>
                      </Col>
                    </Row>
                  </Card>
                  {/* eslint-disable-next-line max-len*/}
                  {state.uploadReactionRes
                    .filter((result: any) => result.sha256 === value.sha256)
                    .map((val: any) => (
                      <Card className="panel" key={val.result.id}>
                        <Row className="reaction-row">
                          <Col md={2}>{val.result.pipeline}</Col>
                          <Col md={2}>{val.result.group}</Col>
                          <Col className={val.result.error ? 'status-error' : ''} md={3}>
                            {val.result.error && val.result.error.split(':')[1]}
                          </Col>
                          <Col>
                            {val.result.error && !state.uploadInProgress && (
                              <Button
                                size={'xsm' as any}
                                variant="no-outline-secondary"
                                className="redo-btn"
                                onClick={() => retrySubmitReaction(val)}
                              >
                                <FaRedo />
                              </Button>
                            )}
                            <Link target="_blank" className="link-text-alt" to={`/reaction/${val.result.group}/${val.result.id}`}>
                              {val.result.id}
                            </Link>
                          </Col>
                        </Row>
                      </Card>
                    ))}
                </div>
              ))}
          </Fragment>
        ))}
      </Row>
      {!state.uploadInProgress ? (
        <Col className="d-flex justify-content-center close-button">
          <Button
            className="ok-btn"
            onClick={(e) => {
              resetStatusMessages();
              setShowUploadStatus(false);
            }}
          >
            Back
          </Button>
        </Col>
      ) : (
        <Col className="d-flex justify-content-center close-button">
          <Button
            className="warning-btn"
            onClick={() => {
              cancelUpload();
            }}
          >
            Cancel
          </Button>
        </Col>
      )}
    </Fragment>
  );
};
