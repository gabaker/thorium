import styled from 'styled-components';

export const GraphWindow = styled.div`
  position: relative;
  background-color: var(--thorium-panel-bg);
  overflow: hidden;
`;

export const GraphDiv = styled.div`
  z-index: 200;
  overflow: hidden;
  min-height: 90vh;
  max-height: 90vh;
`;

export const LoadingOverlay = styled.div`
  position: absolute;
  inset: 0;
  z-index: 400;
  display: flex;
  align-items: center;
  justify-content: center;
  pointer-events: none;
`;

export const PreviewContainer = styled.div`
  position: absolute;
  z-index: 300;
  top: 8px;
  right: 8px;
  background-color: var(--thorium-secondary-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  padding: 10px;
  max-width: 40vw;
  max-height: 30vh;
  overflow-y: auto;
  overflow-x: hidden;
`;

export const PreviewHeader = styled.div`
  display: flex;
  justify-content: flex-end;
  margin-bottom: 4px;
`;

export const PreviewToggleButton = styled.button`
  position: absolute;
  z-index: 300;
  top: 8px;
  right: 8px;
  width: 32px;
  height: 32px;
  border-radius: 8px;
  border: 1px solid var(--thorium-panel-border);
  background: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  display: flex;
  align-items: center;
  justify-content: center;
  cursor: pointer;
  transition: background 0.15s;

  &:hover {
    background: var(--thorium-highlight-panel-bg);
  }
`;

export const MinimizeButton = styled.button`
  background: none;
  border: none;
  color: var(--thorium-secondary-text, var(--thorium-text));
  cursor: pointer;
  padding: 2px;
  display: flex;
  align-items: center;
  border-radius: 4px;
  transition: background 0.15s;

  &:hover {
    background: var(--thorium-highlight-panel-bg);
  }
`;
