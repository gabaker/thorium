import React from 'react';
import { Form } from 'react-bootstrap';

import { ControlRow, ControlLabel } from './Toolbar.styled';

interface LabeledRangeProps {
  id: string;
  label: string;
  value: number;
  min: number;
  max: number;
  step: number;
  formatValue?: (v: number) => string;
  onChange: (value: number) => void;
}

const LabeledRange: React.FC<LabeledRangeProps> = ({ id, label, value, min, max, step, formatValue, onChange }) => (
  <ControlRow>
    <ControlLabel htmlFor={id}>
      {label} ({formatValue ? formatValue(value) : value})
    </ControlLabel>
    <Form.Range
      id={id}
      min={min}
      max={max}
      step={step}
      value={value}
      onChange={(e) => onChange(parseFloat(e.target.value))}
      style={{ flex: 1, minWidth: '100px' }}
    />
  </ControlRow>
);

export default LabeledRange;
