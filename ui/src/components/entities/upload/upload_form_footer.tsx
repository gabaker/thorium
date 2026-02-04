import React from 'react';
import { Button, Col, Row } from 'react-bootstrap';
import { OverlayTipTop } from '@components';
import { useUploadForm } from './upload_context';
import { ProgressBarContainer } from './progress_bar_container';
import { AlertBanner } from './alert_banner';

/**
 * Component for the upload form footer
 * Displays progress bars, alerts, notes, and the upload button
 */
export const UploadFormFooter: React.FC<{ onUpload: () => void }> = ({ onUpload }) => {
  const { state } = useUploadForm();

  return (
    <>
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
          {state.uploadStatus && Object.entries(state.uploadStatus).length > 0 && (
            <Row className="upload-bar mt-3">
              {Object.entries(state.uploadStatus).map(([key, value]) => (
                <OverlayTipTop key={key} tip={value.msg}>
                  {key}
                  <ProgressBarContainer name={key} value={value.progress} error={state.uploadError.length} />
                </OverlayTipTop>
              ))}
            </Row>
          )}
          {!state.uploadInProgress && (
            <>
              <Row className="upload_alerts">
                <Col className="upload-field">
                  <AlertBanner />
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
