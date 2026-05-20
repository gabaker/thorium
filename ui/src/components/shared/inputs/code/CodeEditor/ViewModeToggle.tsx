import React from 'react';
import styled from 'styled-components';

export type ViewMode = 'form' | 'editor';

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

  &:hover {
    background: var(--thorium-info-secondary-bg);
  }
`;

export interface ViewModeToggleProps {
  viewMode: ViewMode;
  onViewModeChange: (mode: ViewMode) => void;
}

const ViewModeToggle: React.FC<ViewModeToggleProps> = ({ viewMode, onViewModeChange }) => (
  <Wrapper>
    <Label>View:</Label>
    <Btn $active={viewMode === 'form'} onClick={() => onViewModeChange('form')}>
      Form
    </Btn>
    <Btn $active={viewMode === 'editor'} onClick={() => onViewModeChange('editor')}>
      Editor
    </Btn>
  </Wrapper>
);

export default ViewModeToggle;
