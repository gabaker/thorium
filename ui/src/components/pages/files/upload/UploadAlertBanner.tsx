import React, { Fragment } from 'react';
import { Alert, Row } from 'react-bootstrap';
import { Link } from 'react-router-dom';
import RunReactionAlerts from '../reactions/RunReactionAlerts';
import { ReactionSubmitResult } from './types';

interface UploadAlertBannerProps {
  uploadSHA256: string[];
  uploadError: string[];
  runReactionsRes: ReactionSubmitResult[];
}

const UploadAlertBanner: React.FC<UploadAlertBannerProps> = ({ uploadSHA256, uploadError, runReactionsRes }) => {
  return (
    <Fragment>
      <Row>
        {uploadSHA256 &&
          uploadSHA256.map((sha256) => (
            <Alert className="d-flex justify-content-center" key={sha256} variant="success">
              File uploaded successfully: <pre> </pre>
              <Link className="link-text" to={'/file/' + sha256} target="_blank">
                {sha256}
              </Link>
            </Alert>
          ))}
      </Row>
      <Row>
        {uploadError &&
          uploadError.map((message) => (
            <Alert className="d-flex justify-content-center" key={message} variant="danger">
              {message}
            </Alert>
          ))}
      </Row>
      <RunReactionAlerts responses={runReactionsRes} />
    </Fragment>
  );
};

export default UploadAlertBanner;
