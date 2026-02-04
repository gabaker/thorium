import React, { Fragment } from 'react';
import { Alert, Row } from 'react-bootstrap';
import { Link } from 'react-router-dom';
import { RunReactionAlerts } from '@components';
import { useUploadForm } from './upload_context';

/**
 * Component for displaying upload alerts and errors
 * Shows success messages with file SHA256 links and error messages
 */
export const AlertBanner: React.FC = () => {
  const { state } = useUploadForm();

  return (
    <Fragment>
      <Row>
        {state.uploadSHA256 &&
          state.uploadSHA256.map((sha256) => (
            <Alert className="d-flex justify-content-center" key={sha256} variant="success">
              File uploaded successfully: <pre> </pre>
              <Link className="link-text" to={'/file/' + sha256} target="_blank">
                {sha256}
              </Link>
            </Alert>
          ))}
      </Row>
      <Row>
        {state.uploadError &&
          state.uploadError.map((message) => (
            <Alert className="d-flex justify-content-center" key={message} variant="danger">
              {message}
            </Alert>
          ))}
      </Row>
      <RunReactionAlerts responses={state.runReactionsRes} />
    </Fragment>
  );
};
