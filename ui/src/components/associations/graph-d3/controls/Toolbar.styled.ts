import styled, { css } from 'styled-components';
import { Popover } from 'react-bootstrap';

export const ToolbarContainer = styled.div`
  position: absolute;
  bottom: 12px;
  left: 12px;
  z-index: 500;
  display: flex;
  align-items: center;
  gap: 6px;
`;

export const ToolbarIconButton = styled.button<{ $active?: boolean }>`
  width: 36px;
  height: 36px;
  border-radius: 8px;
  border: 1px solid var(--thorium-panel-border);
  background: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  display: flex;
  align-items: center;
  justify-content: center;
  cursor: pointer;
  transition: background 0.15s, border-color 0.15s;

  &:hover {
    background: var(--thorium-highlight-panel-bg);
  }

  ${({ $active }) =>
    $active &&
    css`
      background: var(--thorium-highlight-panel-bg);
      border-color: var(--thorium-highlight-panel-border);
    `}
`;

export const NodeCount = styled.span`
  font-size: 0.75rem;
  color: var(--thorium-secondary-text);
  padding: 0 8px;
  white-space: nowrap;
  user-select: none;
`;

export const StyledPopover = styled(Popover)`
  --bs-popover-bg: var(--thorium-secondary-panel-bg);
  --bs-popover-border-color: var(--thorium-panel-border);
  --bs-popover-header-bg: var(--thorium-highlight-panel-bg);
  --bs-popover-header-color: var(--thorium-text);
  --bs-popover-body-color: var(--thorium-text);
  --bs-popover-arrow-border: var(--thorium-panel-border);

  .popover-header {
    color: var(--thorium-text);
    border-bottom: 1px solid var(--thorium-panel-border);
  }
`;

export const PopoverBody = styled.div`
  display: grid;
  gap: 10px;
  padding: 4px;
  min-width: 200px;
`;

export const ControlRow = styled.div`
  display: flex;
  align-items: center;
  gap: 8px;
`;

export const ControlLabel = styled.label`
  font-size: 0.8rem;
  white-space: nowrap;
  color: var(--thorium-text);
  min-width: 0;
`;

export const RangeInput = styled.input.attrs({ type: 'range' })`
  flex: 1;
  min-width: 100px;
`;

export const Divider = styled.hr`
  margin: 2px 0;
  border-color: var(--thorium-panel-border);
  opacity: 0.4;
`;
