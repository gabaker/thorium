import styled from 'styled-components';

/** Centered row that hosts an {@link ExpandToggle}. */
export const ToggleRow = styled.div`
  display: flex;
  justify-content: center;
  padding-top: 8px;
`;

/**
 * Themed, transparent "expand/collapse" toggle — a caret icon plus a label. Shared by the tool-result body
 * collapse and the entity browser's inline "details" toggle so both read the same. The caret icon and label
 * are passed as children (state is encoded by which caret the caller renders), so this needs no variant prop.
 */
export const ExpandToggle = styled.button`
  display: inline-flex;
  align-items: center;
  gap: 6px;
  background: transparent;
  border: none;
  color: var(--thorium-highlight-text);
  font-size: 0.85rem;
  font-weight: 600;
  cursor: pointer;
  padding: 4px 10px;
  border-radius: 6px;

  &:hover {
    color: var(--thorium-text);
    background: var(--thorium-highlight-panel-bg);
  }

  &:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--thorium-highlight-text);
  }
`;
