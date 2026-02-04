import React from 'react';
import { Col, Form, Row } from 'react-bootstrap';
import { Subtitle } from '@components';
import { useUploadForm } from './upload_context';

type OriginFormFieldProps = {
  label: string;
  type?: string;
  value: any;
  placeholder: string;
  onChange: (value: any) => void;
  isInvalid?: any;
  invalidMessage?: string;
};

/**
 * Reusable form field component for origin forms
 * Automatically resets status messages on change via context
 */
export const OriginFormField: React.FC<OriginFormFieldProps> = ({
  label,
  type = 'text',
  value,
  placeholder,
  onChange,
  isInvalid,
  invalidMessage,
}) => {
  const { dispatch } = useUploadForm();

  const handleChange = (newValue: any) => {
    onChange(newValue);
    dispatch({ type: 'RESET_STATUS_MESSAGES' });
  };

  return (
    <>
      <br />
      <Row>
        <Col className="name-width" xs={2}>
          <Subtitle>{label}</Subtitle>
        </Col>
        <Col xs={5}>
          <Form.Control
            type={type}
            value={value}
            placeholder={placeholder}
            onChange={(e) => handleChange(e.target.value)}
            isInvalid={!!isInvalid}
          />
          {invalidMessage && <Form.Control.Feedback type="invalid">{invalidMessage}</Form.Control.Feedback>}
        </Col>
      </Row>
    </>
  );
};
