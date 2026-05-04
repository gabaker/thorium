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

export const DataPreview = styled.div`
  position: absolute;
  z-index: 300;
  top: 0px;
  right: 0px;
  background-color: rgba(255, 255, 255, 0.01) !important;
  padding: 10px;
  max-width: 40vw;
  max-height: 30vh;
  overflow-y: auto;
  overflow-x: auto;
`;
