import React from 'react';
import { ProgressBar } from 'react-bootstrap';

type ProgressBarProps = {
  name: string;
  value: number;
  error: any;
};

// This container allows the progress bar color to change depending on error conditions.
export const ProgressBarContainer: React.FC<ProgressBarProps> = ({ name, value, error }) => {
  return (
    <>
      {value < 100 && (
        <ProgressBar animated key={name} label={value} now={value} className={error ? 'warning-bar' : 'info-bar'}></ProgressBar>
      )}
      {value >= 100 && <ProgressBar key={name} label={value} now={value} className={error ? 'danger-bar' : 'success-bar'}></ProgressBar>}
    </>
  );
};
