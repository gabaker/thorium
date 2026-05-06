import React, { Fragment } from 'react';
import { Button, Card, Col, Row } from 'react-bootstrap';
import { FaRedo } from 'react-icons/fa';
import { OverlayTipTop } from '@components/shared/overlay/tips';
import ProgressBarContainer from './ProgressBarContainer';
import UploadStatusTable from './UploadStatusTable';
import { FileUploadStatus, ReactionResultEntry } from './types';

interface UploadStatusDashboardProps {
  uploadInProgress: boolean;
  activeUploads: string[];
  uploadStatus: Record<string, FileUploadStatus>;
  uploadFailures: Record<string, FormData>;
  uploadStatusDropdown: Record<string, boolean>;
  setUploadStatusDropdown: React.Dispatch<React.SetStateAction<Record<string, boolean>>>;
  uploadReactionRes: ReactionResultEntry[];
  uploadReactions: Record<string, any[]>;
  uploadReactionFailures: number;
  uploadError: string[];
  totalProgress: number;
  onRetryAllFileUploads: () => void;
  onRetryAllReactionSubmissions: () => void;
  onRetryFileUpload: (fileName: string) => void;
  onRetrySubmitReaction: (status: ReactionResultEntry) => void;
  onBack: () => void;
  onCancel: () => void;
}

const UploadStatusDashboard: React.FC<UploadStatusDashboardProps> = ({
  uploadInProgress,
  activeUploads,
  uploadStatus,
  uploadFailures,
  uploadStatusDropdown,
  setUploadStatusDropdown,
  uploadReactionRes,
  uploadReactions,
  uploadReactionFailures,
  uploadError,
  totalProgress,
  onRetryAllFileUploads,
  onRetryAllReactionSubmissions,
  onRetryFileUpload,
  onRetrySubmitReaction,
  onBack,
  onCancel,
}) => {
  return (
    <Fragment>
      Total
      <Row className="upload-bar">
        <Col>
          <ProgressBarContainer name={'Total'} value={totalProgress} error={uploadError.length > 0} />
        </Col>
      </Row>
      {uploadInProgress && (
        <Row className="upload-bar">
          {Object.values(activeUploads).map((key) => {
            const status = uploadStatus[key];
            if (!status) return null;
            return (
              <OverlayTipTop key={key} tip={status.msg}>
                {key}
                <ProgressBarContainer name={key} value={status.progress} error={status.fileFail} />
              </OverlayTipTop>
            );
          })}
        </Row>
      )}
      {!uploadInProgress && (
        <Card className="stats-container panel">
          <Card.Body>
            <div>{Object.keys(uploadStatus).length - Object.keys(uploadFailures).length} Files Uploaded Successfully</div>
            {Object.keys(uploadFailures).length > 0 && (
              <div>
                {Object.keys(uploadFailures).length} File Upload Failure(s)
                <Button size={'xsm' as any} variant="no-outline-secondary" className="retry-button" onClick={onRetryAllFileUploads}>
                  {' '}
                  <FaRedo />
                </Button>
              </div>
            )}
            <div>{uploadReactionRes.length - uploadReactionFailures} Reaction(s) Submitted Successfully</div>
            {uploadReactionFailures > 0 && (
              <div>
                {uploadReactionFailures} Reaction Submission(s) Failed
                <Button size={'xsm' as any} variant="no-outline-secondary" className="retry-button" onClick={onRetryAllReactionSubmissions}>
                  {' '}
                  <FaRedo />
                </Button>
              </div>
            )}
          </Card.Body>
        </Card>
      )}
      <UploadStatusTable
        uploadStatus={uploadStatus}
        uploadStatusDropdown={uploadStatusDropdown}
        setUploadStatusDropdown={setUploadStatusDropdown}
        uploadReactions={uploadReactions}
        uploadReactionRes={uploadReactionRes}
        uploadInProgress={uploadInProgress}
        onRetryFileUpload={onRetryFileUpload}
        onRetrySubmitReaction={onRetrySubmitReaction}
      />
      {!uploadInProgress ? (
        <Col className="d-flex justify-content-center close-button">
          <Button className="ok-btn" onClick={onBack}>
            Back
          </Button>
        </Col>
      ) : (
        <Col className="d-flex justify-content-center close-button">
          <Button className="warning-btn" onClick={onCancel}>
            Cancel
          </Button>
        </Col>
      )}
    </Fragment>
  );
};

export default UploadStatusDashboard;
