import styled from 'styled-components';

// spec: ./AssociationGraph.spec.md

export const GraphWindow = styled.div<{ $bordered?: boolean }>`
  position: relative;
  background-color: var(--thorium-panel-bg);
  ${({ $bordered = true }) =>
    $bordered &&
    `
    border: 1px solid var(--thorium-panel-border);
    border-radius: 4px;
  `}
  overflow: hidden;
`;

export const GraphDiv = styled.div`
  z-index: 200;
  overflow: hidden;
  min-height: 90vh;
  max-height: 90vh;
`;

export const TreeOverlayPanel = styled.div`
  position: absolute;
  z-index: 300;
  top: 8px;
  left: 8px;
  max-width: 30%;
  max-height: 60vh;
  min-width: 200px;
  background: color-mix(in srgb, var(--thorium-secondary-panel-bg) 82%, transparent);
  backdrop-filter: blur(8px);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  overflow-y: auto;
  overflow-x: hidden;
  display: flex;
  flex-direction: column;

  @media (max-width: 768px) {
    max-width: calc(100% - 16px);
  }
`;

export const TreeOverlayHeader = styled.div`
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 8px 10px 4px;
  font-size: 0.8rem;
  font-weight: 600;
  color: var(--thorium-text);
  position: sticky;
  top: 0;
  background: inherit;
  z-index: 1;
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
  background: color-mix(in srgb, var(--thorium-secondary-panel-bg) 90%, transparent);
  backdrop-filter: blur(8px);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  width: fit-content;
  min-width: 240px;
  max-width: min(400px, 35vw);
`;

// scrollable content region of the preview overlay. Scrolling lives here (not on PreviewContainer) so
// the absolutely-positioned collapse button stays pinned to the corner instead of scrolling with content
export const PreviewScroll = styled.div`
  padding: 2px;
  max-height: 30vh;
  overflow-y: auto;
  overflow-x: auto;
`;

// collapse control floating above the overlay's top-right corner rather than occupying its own header
// row. The overlay's leading line is a short uppercase kind label at the top-left, so this corner stays
// clear and the button won't overlap the title
export const PreviewCollapseButton = styled.button`
  position: absolute;
  /* nudged down to visually align with the summary's first subtitle, which is inset by the
     EntitySummary wrapper's 0.5rem top margin */
  top: 8px;
  right: 4px;
  z-index: 1;
  width: 20px;
  height: 20px;
  background: none;
  border: none;
  color: var(--thorium-secondary-text, var(--thorium-text));
  cursor: pointer;
  padding: 2px;
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: 4px;
  transition: background 0.15s;

  &:hover {
    background: var(--thorium-highlight-panel-bg);
  }
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

export const TreeOverlayToggle = styled.button`
  position: absolute;
  z-index: 300;
  top: 8px;
  left: 8px;
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
