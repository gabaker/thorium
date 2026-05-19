import React from 'react';
import styled from 'styled-components';
import type { FormatType } from '@utilities/rules/types';

const Wrapper = styled.div`
  display: flex;
  gap: 8px;
  align-items: center;
`;

const Label = styled.span`
  font-size: 12px;
  color: var(--thorium-secondary-text);
  font-weight: 600;
`;

const Btn = styled.button<{ $active: boolean }>`
  padding: 4px 12px;
  border: 1px solid var(--thorium-panel-border);
  border-radius: 3px;
  background: ${(props) => (props.$active ? 'var(--thorium-info-secondary-bg)' : 'transparent')};
  color: var(--thorium-text);
  cursor: pointer;
  font-size: 12px;
  font-family: monospace;

  &:hover {
    background: var(--thorium-info-secondary-bg);
  }
`;

export interface FormatToggleProps {
  format: FormatType;
  onFormatChange: (format: FormatType) => void;
}

const FormatToggle: React.FC<FormatToggleProps> = ({ format, onFormatChange }) => (
  <Wrapper>
    <Label>Format:</Label>
    <Btn $active={format === 'yaml'} onClick={() => onFormatChange('yaml')}>
      YAML
    </Btn>
    <Btn $active={format === 'json'} onClick={() => onFormatChange('json')}>
      JSON
    </Btn>
  </Wrapper>
);

export default FormatToggle;
