import React, { Fragment, useState, useEffect } from 'react';
import { Col, Form, Row } from 'react-bootstrap';
import { FaQuestionCircle } from 'react-icons/fa';

// Project imports
import { FieldBadge, OverlayTipRight, Subtitle } from '@components';

// Tooltips provide contextual help for each cleanup configuration field
const CleanupToolTips = {
  self: `Cleanup configuration allows specifying a script that runs when a job is canceled.
    This enables graceful cleanup of resources and state when jobs are terminated early.`,
  script: `The path to the cleanup script that will be executed when a job is canceled.
    This script should handle any necessary cleanup operations.`,
  job_id: `How to pass the job ID to the cleanup script. Options are None (don't pass),
    Append (add as positional argument), or Kwarg (pass with specified flag).`,
  results: `How to pass the results path to the cleanup script. Options are None (don't pass),
    Append (add as positional argument), or Kwarg (pass with specified flag).`,
  result_files_dir: `How to pass the result files directory path to the cleanup script.
    Options are None (don't pass), Append (add as positional argument), or Kwarg (pass with specified flag).`,
};

// ArgStrategy defines how arguments are passed to the cleanup script
type ArgStrategy = 'None' | 'Append' | { Kwarg: string };

// Cleanup interface matches the API Cleanup struct
interface Cleanup {
  script: string;
  job_id: ArgStrategy;
  results: ArgStrategy;
  result_files_dir: ArgStrategy;
}

// CleanupUpdate interface for edit mode with clear flag
interface CleanupUpdate {
  script?: string;
  job_id?: ArgStrategy;
  results?: ArgStrategy;
  result_files_dir?: ArgStrategy;
  clear?: boolean;
}

// Template provides default values for creating new cleanup configurations
const CleanupTemplate: Cleanup = {
  script: '',
  job_id: 'None',
  results: 'None',
  result_files_dir: 'None',
};

// Available argument strategy types for dropdown selection
const ArgStrategyTypes = ['None', 'Append', 'Kwarg'] as const;

// Helper to extract the strategy type from an ArgStrategy value
const getStrategyType = (strategy: ArgStrategy): string => {
  if (typeof strategy === 'string') {
    return strategy;
  }
  return 'Kwarg';
};

// Helper to extract the kwarg value from an ArgStrategy
const getKwargValue = (strategy: ArgStrategy): string => {
  if (typeof strategy === 'object' && 'Kwarg' in strategy) {
    return strategy.Kwarg;
  }
  return '';
};

// Props interface for the ArgStrategyInput component
interface ArgStrategyInputProps {
  label: string;
  tooltip: string;
  value: ArgStrategy;
  onChange: (newValue: ArgStrategy) => void;
}

// ArgStrategyInput renders a strategy selector with optional kwarg input
const ArgStrategyInput: React.FC<ArgStrategyInputProps> = ({ label, tooltip, value, onChange }) => {
  const strategyType = getStrategyType(value);
  const kwargValue = getKwargValue(value);

  // Handle strategy type change
  const handleTypeChange = (newType: string) => {
    if (newType === 'Kwarg') {
      onChange({ Kwarg: kwargValue || '' });
    } else {
      onChange(newType as 'None' | 'Append');
    }
  };

  // Handle kwarg value change
  const handleKwargChange = (newKwarg: string) => {
    onChange({ Kwarg: newKwarg });
  };

  return (
    <Row className="mb-2">
      <Col className="key-col-2-ext">
        <em>{`${label}: `}</em>
      </Col>
      <Col className="key-col-3">
        <div className="image-fields">
          <OverlayTipRight tip={tooltip}>
            <Form.Group className="mb-2 image-fields">
              <Row>
                <Col className="resource-type-col">
                  <Form.Select value={strategyType} onChange={(e) => handleTypeChange(e.target.value)}>
                    {ArgStrategyTypes.map((type) => (
                      <option key={type} value={type}>
                        {type}
                      </option>
                    ))}
                  </Form.Select>
                </Col>
                {strategyType === 'Kwarg' && (
                  <Col className="resource-type-col">
                    <Form.Control
                      type="text"
                      value={kwargValue}
                      placeholder="--flag"
                      onChange={(e) => handleKwargChange(e.target.value)}
                    />
                  </Col>
                )}
              </Row>
            </Form.Group>
          </OverlayTipRight>
        </div>
      </Col>
    </Row>
  );
};

// Props interface for DisplayCleanup component
interface DisplayCleanupProps {
  cleanup: Cleanup | null;
}

// DisplayCleanup renders cleanup configuration in view mode
const DisplayCleanup: React.FC<DisplayCleanupProps> = ({ cleanup }) => {
  // Format ArgStrategy for display
  const formatStrategy = (strategy: ArgStrategy): string => {
    if (typeof strategy === 'string') {
      return strategy;
    }
    return `Kwarg: ${strategy.Kwarg}`;
  };

  if (!cleanup) {
    return (
      <Fragment>
        <Row>
          <Col>
            <OverlayTipRight tip={CleanupToolTips.self}>
              <b>{'Cleanup'}</b> <FaQuestionCircle />
            </OverlayTipRight>
          </Col>
        </Row>
        <Row>
          <Col className="key-col-1" />
          <Col>
            <em>Not configured</em>
          </Col>
        </Row>
      </Fragment>
    );
  }

  return (
    <Fragment>
      <Row>
        <Col>
          <OverlayTipRight tip={CleanupToolTips.self}>
            <b>{'Cleanup'}</b> <FaQuestionCircle />
          </OverlayTipRight>
        </Col>
      </Row>
      <Row>
        <Col className="key-col-1" />
        <Col className="key-col-2-ext">
          <em>{`script: `}</em>
        </Col>
        <Col className="key-col-3">
          <div className="image-fields">
            <OverlayTipRight tip={CleanupToolTips.script}>
              <FieldBadge field={cleanup.script} color={'#7e7c7c'} />
            </OverlayTipRight>
          </div>
        </Col>
      </Row>
      <Row>
        <Col className="key-col-1" />
        <Col className="key-col-2-ext">
          <em>{`job_id: `}</em>
        </Col>
        <Col className="key-col-3">
          <div className="image-fields">
            <OverlayTipRight tip={CleanupToolTips.job_id}>
              <FieldBadge field={formatStrategy(cleanup.job_id)} color={'#7e7c7c'} />
            </OverlayTipRight>
          </div>
        </Col>
      </Row>
      <Row>
        <Col className="key-col-1" />
        <Col className="key-col-2-ext">
          <em>{`results: `}</em>
        </Col>
        <Col className="key-col-3">
          <div className="image-fields">
            <OverlayTipRight tip={CleanupToolTips.results}>
              <FieldBadge field={formatStrategy(cleanup.results)} color={'#7e7c7c'} />
            </OverlayTipRight>
          </div>
        </Col>
      </Row>
      <Row>
        <Col className="key-col-1" />
        <Col className="key-col-2-ext">
          <em>{`result_files_dir: `}</em>
        </Col>
        <Col className="key-col-3">
          <div className="image-fields">
            <OverlayTipRight tip={CleanupToolTips.result_files_dir}>
              <FieldBadge field={formatStrategy(cleanup.result_files_dir)} color={'#7e7c7c'} />
            </OverlayTipRight>
          </div>
        </Col>
      </Row>
    </Fragment>
  );
};

// Props interface for CleanupInputs component
interface CleanupInputsProps {
  initialCleanup: Cleanup;
  setRequestCleanup: (cleanup: Cleanup) => void;
}

// CleanupInputs renders the form inputs for editing cleanup configuration
const CleanupInputs: React.FC<CleanupInputsProps> = ({ initialCleanup, setRequestCleanup }) => {
  const [cleanup, setCleanup] = useState<Cleanup>(structuredClone(initialCleanup));

  // Update cleanup state and propagate to parent
  const updateCleanup = <K extends keyof Cleanup>(key: K, value: Cleanup[K]) => {
    const cleanupCopy = structuredClone(cleanup);
    cleanupCopy[key] = value;
    setCleanup(cleanupCopy);
    setRequestCleanup(cleanupCopy);
  };

  // Initialize parent state on mount
  useEffect(() => {
    setRequestCleanup(initialCleanup);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <Fragment>
      {/* Script path input */}
      <Row className="mb-2">
        <Col className="key-col-2-ext">
          <em>{`script: `}</em>
        </Col>
        <Col className="key-col-3">
          <div className="image-fields">
            <OverlayTipRight tip={CleanupToolTips.script}>
              <Form.Group className="mb-2 image-fields">
                <Form.Control
                  type="text"
                  value={cleanup.script}
                  placeholder="/path/to/cleanup.sh"
                  onChange={(e) => updateCleanup('script', e.target.value)}
                />
              </Form.Group>
            </OverlayTipRight>
          </div>
        </Col>
      </Row>
      {/* Job ID strategy input */}
      <ArgStrategyInput
        label="job_id"
        tooltip={CleanupToolTips.job_id}
        value={cleanup.job_id}
        onChange={(value) => updateCleanup('job_id', value)}
      />
      {/* Results strategy input */}
      <ArgStrategyInput
        label="results"
        tooltip={CleanupToolTips.results}
        value={cleanup.results}
        onChange={(value) => updateCleanup('results', value)}
      />
      {/* Result files directory strategy input */}
      <ArgStrategyInput
        label="result_files_dir"
        tooltip={CleanupToolTips.result_files_dir}
        value={cleanup.result_files_dir}
        onChange={(value) => updateCleanup('result_files_dir', value)}
      />
    </Fragment>
  );
};

// Props interface for CreateImageCleanup component
interface CreateImageCleanupProps {
  cleanup: Cleanup;
  setRequestCleanup: (cleanup: Cleanup) => void;
}

// CreateImageCleanup wraps CleanupInputs for the create page layout
const CreateImageCleanup: React.FC<CreateImageCleanupProps> = ({ cleanup, setRequestCleanup }) => {
  return (
    <Row>
      <Col className="title-col">
        <h5>Cleanup</h5>
      </Col>
      <Col className="field-col">
        <CleanupInputs initialCleanup={cleanup} setRequestCleanup={setRequestCleanup} />
      </Col>
    </Row>
  );
};

// Props interface for EditImageCleanup component
interface EditImageCleanupProps {
  cleanup: Cleanup;
  setRequestCleanup: (cleanup: Cleanup) => void;
}

// EditImageCleanup wraps CleanupInputs for the edit page layout
const EditImageCleanup: React.FC<EditImageCleanupProps> = ({ cleanup, setRequestCleanup }) => {
  return (
    <Row>
      <Row>
        <Col className="field-name-col-ext">
          <OverlayTipRight tip={CleanupToolTips.self}>
            <b>{'Cleanup'}</b> <FaQuestionCircle />
          </OverlayTipRight>
        </Col>
      </Row>
      <Row>
        <Col className="key-col-1" />
        <Col>
          <CleanupInputs initialCleanup={cleanup} setRequestCleanup={setRequestCleanup} />
        </Col>
      </Row>
    </Row>
  );
};

// Update create request to format cleanup for API submission
export const updateCreateRequestCleanup = (
  cleanup: Cleanup,
  setRequestCleanup: (cleanup: Cleanup | null) => void
): void => {
  // If script is empty, don't include cleanup in request
  if (!cleanup.script || cleanup.script.trim() === '') {
    setRequestCleanup(null);
    return;
  }
  setRequestCleanup(cleanup);
};

// Update edit request to format cleanup for API submission
export const updateEditRequestCleanup = (
  cleanup: Cleanup,
  setRequestCleanup: (cleanup: CleanupUpdate) => void
): void => {
  // If script is empty, set clear flag to remove existing cleanup
  if (!cleanup.script || cleanup.script.trim() === '') {
    setRequestCleanup({ clear: true });
    return;
  }
  setRequestCleanup(cleanup);
};

// Props interface for the main ImageCleanup component
interface ImageCleanupProps {
  cleanup: Cleanup | null;
  setRequestCleanup: (cleanup: Cleanup | CleanupUpdate | null) => void;
  mode: 'View' | 'Edit' | 'Create' | 'Copy';
}

// ImageCleanup is the main component that switches between view, edit, and create modes
const ImageCleanup: React.FC<ImageCleanupProps> = ({ cleanup, setRequestCleanup, mode }) => {
  // Wrapper to format cleanup for API request based on mode
  const setUpdatedCleanup = (newCleanup: Cleanup) => {
    if (['Create', 'Copy'].includes(mode)) {
      return updateCreateRequestCleanup(newCleanup, setRequestCleanup);
    } else {
      return updateEditRequestCleanup(newCleanup, setRequestCleanup);
    }
  };

  // For Copy mode, use existing cleanup or template
  if (mode === 'Copy') {
    return <CreateImageCleanup cleanup={cleanup || CleanupTemplate} setRequestCleanup={setUpdatedCleanup} />;
  }

  // For Create mode, use template
  if (mode === 'Create') {
    return <CreateImageCleanup cleanup={CleanupTemplate} setRequestCleanup={setUpdatedCleanup} />;
  }

  // For View mode, display the cleanup configuration
  if (mode === 'View') {
    return <DisplayCleanup cleanup={cleanup} />;
  }

  // For Edit mode, show edit form with existing or default values
  return <EditImageCleanup cleanup={cleanup || CleanupTemplate} setRequestCleanup={setUpdatedCleanup} />;
};

export default ImageCleanup;

// Export template for use in parent components
export { CleanupTemplate };
export type { Cleanup, CleanupUpdate, ArgStrategy };
