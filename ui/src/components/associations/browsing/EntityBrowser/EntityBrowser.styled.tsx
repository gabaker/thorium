import styled, { css, keyframes } from 'styled-components';

// project imports
import { BUTTON_BAR_GAP } from '@components/shared/buttons/tokens';

const spin = keyframes`
  to { transform: rotate(360deg); }
`;

/** Tiny inline spinner shown on a row while its node is being grown. */
export const RowSpinner = styled.span`
  flex: 0 0 auto;
  width: 12px;
  height: 12px;
  border: 2px solid var(--thorium-panel-border);
  border-top-color: var(--thorium-highlight-text);
  border-radius: 50%;
  animation: ${spin} 0.7s linear infinite;
`;

/** Outer container for the whole browser. */
export const BrowserRoot = styled.div`
  display: flex;
  flex-direction: column;
  gap: 10px;
  /* small end gap so the last tree item doesn't butt against the container edge */
  padding-bottom: 10px;
`;

// --- toolbar ---

export const ToolbarBar = styled.div`
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  padding: 8px 10px;
  background: var(--thorium-secondary-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
`;

/** Flex slot that lets the omnibar grow to fill the toolbar while the Flagged toggle sits beside it. */
export const OmnibarSlot = styled.div`
  flex: 1 1 320px;
  min-width: 220px;
`;

export const ToggleChip = styled.button<{ $active?: boolean }>`
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 4px 10px;
  font-size: 0.8rem;
  font-weight: 600;
  border-radius: 12px;
  cursor: pointer;
  color: ${({ $active }) => ($active ? 'var(--thorium-button-text)' : 'var(--thorium-secondary-text)')};
  background: ${({ $active }) => ($active ? 'var(--thorium-danger-bg)' : 'var(--thorium-panel-bg)')};
  border: 1px solid ${({ $active }) => ($active ? 'var(--thorium-danger-bg)' : 'var(--thorium-panel-border)')};

  &:hover {
    border-color: var(--thorium-highlight-panel-border);
  }
`;

/** Positioning context for the hidden-nodes chip + its dropdown. */
export const HiddenControl = styled.div`
  position: relative;
  display: inline-flex;
`;

/** Popover listing hidden node labels for per-item unhide, anchored under the chip. */
export const HiddenMenu = styled.div`
  position: absolute;
  top: calc(100% + 4px);
  right: 0;
  z-index: 20;
  min-width: 220px;
  max-width: 320px;
  max-height: 280px;
  overflow-y: auto;
  padding: 4px;
  background: var(--thorium-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.25);
`;

/** A row within the hidden-nodes menu: the node label plus an unhide button. */
export const HiddenMenuItem = styled.div`
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 4px 6px;
  font-size: 0.8rem;
  color: var(--thorium-text);
  border-radius: 6px;

  &:hover {
    background: var(--thorium-highlight-panel-bg);
  }
`;

/** The label span in a hidden-menu row (ellipsized). */
export const HiddenMenuLabel = styled.span`
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
`;

/** A small text button used for per-item unhide and clear-all inside the hidden control. */
export const HiddenMenuAction = styled.button`
  flex: 0 0 auto;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  background: transparent;
  border: none;
  color: var(--thorium-highlight-text);
  font-size: 0.78rem;
  font-weight: 600;
  cursor: pointer;
  padding: 2px 6px;
  border-radius: 6px;

  &:hover {
    background: var(--thorium-highlight-panel-bg);
    color: var(--thorium-text);
  }
  &:focus-visible {
    outline: 2px solid var(--thorium-highlight-text);
    outline-offset: -2px;
  }
`;

/** A dividing header row inside the hidden-nodes menu (clear-all lives here). */
export const HiddenMenuHeader = styled.div`
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 4px 6px;
  border-bottom: 1px solid var(--thorium-panel-border);
  margin-bottom: 4px;
  color: var(--thorium-secondary-text);
  font-size: 0.78rem;
  font-weight: 700;
`;

/** Right-aligned slot that anchors the hover/focus-revealed hide affordance at the header's trailing edge. */
export const HideSlot = styled.span`
  flex: 0 0 auto;
  margin-left: auto;
  display: inline-flex;
`;

/** Hover/focus-revealed "hide this item" affordance in a row header (eye-slash). */
export const HideButton = styled.button`
  flex: 0 0 auto;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 22px;
  height: 22px;
  background: transparent;
  border: none;
  border-radius: 6px;
  color: var(--thorium-secondary-text);
  cursor: pointer;
  /* revealed only on row hover / keyboard focus-within (see RowHeader) */
  opacity: 0;
  transition: opacity 0.12s ease;

  &:hover {
    background: var(--thorium-highlight-panel-bg);
    color: var(--thorium-text);
  }
  &:focus-visible {
    opacity: 1;
    outline: 2px solid var(--thorium-highlight-text);
    outline-offset: -2px;
  }
`;

// --- tree levels / rows ---

/** A nested level; the left guide-rule keeps deep DAGs readable. */
export const Level = styled.div<{ $depth: number }>`
  display: flex;
  flex-direction: column;
  /* very small margin between listed entities to keep the tree compact */
  gap: 2px;
  /* taper the indent step past depth 4 so deeply nested rows keep usable header width */
  margin-left: ${({ $depth }) => ($depth > 0 ? ($depth > 4 ? 6 : 10) : 0)}px;
  padding-left: ${({ $depth }) => ($depth > 0 ? ($depth > 4 ? 6 : 10) : 0)}px;
  border-left: ${({ $depth }) => ($depth > 0 ? '1px solid var(--thorium-panel-border)' : 'none')};
`;

export const GroupHeaderRow = styled.div`
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 6px;
  color: var(--thorium-secondary-text);
  font-size: 0.8rem;
  font-weight: 700;
  text-transform: uppercase;
  letter-spacing: 0.03em;
`;

export const GroupCount = styled.span`
  font-variant-numeric: tabular-nums;
  color: var(--thorium-secondary-text);
`;

/** Wraps a row's header + expanded body so they group as a single flex item within a level. */
export const RowContainer = styled.div`
  display: flex;
  flex-direction: column;
  /* same small gap between a node's info box and its nested children level as between sibling entities */
  gap: 2px;
`;

/** A single entity/file/repo "info box": header + metadata preview grouped in one bordered card. */
export const InfoBox = styled.div`
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  background: var(--thorium-panel-bg);
  overflow: hidden;
`;

export const RowHeader = styled.div`
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 10px;
  cursor: pointer;
  color: var(--thorium-text);

  &:hover {
    background: var(--thorium-highlight-panel-bg);
  }
  &:focus-visible {
    outline: 2px solid var(--thorium-highlight-text);
    outline-offset: -2px;
  }
  /* reveal the hide affordance when the row is hovered or anything inside it has keyboard focus */
  &:hover ${HideButton}, &:focus-within ${HideButton} {
    opacity: 1;
  }
`;

export const Chevron = styled.span<{ $expanded: boolean; $hidden?: boolean }>`
  display: inline-flex;
  align-items: center;
  width: 12px;
  flex: 0 0 auto;
  color: var(--thorium-secondary-text);
  visibility: ${({ $hidden }) => ($hidden ? 'hidden' : 'visible')};
  transform: rotate(${({ $expanded }) => ($expanded ? '90deg' : '0deg')});
  transition: transform 0.15s ease;
`;

/* Shared name styling: the name is the header's growing element with a legible floor,
   so it ellipsizes instead of losing the flex shrink fight against the badges. */
const identifierBase = css`
  flex: 1 1 8rem;
  min-width: 8rem;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 600;
`;

export const IdentifierLink = styled.a`
  ${identifierBase}
  color: var(--thorium-link-text);
  text-decoration: none;

  &:hover {
    color: var(--thorium-highlight-text);
  }
`;

export const IdentifierText = styled.span`
  ${identifierBase}
  color: var(--thorium-text);
`;

/** Groups the header badges with the shared button-bar/badge gap between them (tighter than the header gap). */
export const BadgeGroup = styled.span`
  display: inline-flex;
  align-items: center;
  flex-wrap: wrap;
  gap: ${BUTTON_BAR_GAP};
  min-width: 0;
`;

const BaseBadge = styled.span`
  flex: 0 0 auto;
  font-size: 0.72rem;
  font-weight: 600;
  padding: 1px 7px;
  border-radius: 10px;
  background: var(--thorium-secondary-panel-bg);
  color: var(--thorium-secondary-text);
`;

export const KindBadge = styled(BaseBadge)``;

export const RelationshipBadge = styled(BaseBadge)`
  background: var(--thorium-highlight-panel-bg);
  color: var(--thorium-highlight-text);
  // "… In <container> <Kind>" labels can get long — cap and ellipsize (full text in the title attr);
  // the percentage bound keeps the badge from eating narrow (e.g. two-column dashboard) headers
  max-width: min(20rem, 55%);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
`;

export const ViaBadge = styled(BaseBadge)`
  background: transparent;
  border: 1px dashed var(--thorium-panel-border);
  font-style: italic;
  font-weight: 500;
  /* breadcrumbs grow with depth — cap and ellipsize (full path stays in the title attr) */
  max-width: 14rem;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
`;

export const DuplicateBadge = styled(BaseBadge)`
  background: var(--thorium-warning-secondary-bg);
  color: var(--thorium-warning-text);
`;

/** Indicates a node has more associations that can be fetched (grown) from the server. */
export const GrowBadge = styled(BaseBadge)`
  display: inline-flex;
  align-items: center;
  gap: 3px;
  background: var(--thorium-ok-bg);
  color: var(--thorium-button-text);
`;

/** Small aggregate badge on a layer header (e.g. danger / ATT&CK counts). */
export const AggBadge = styled(BaseBadge)<{ $danger?: boolean }>`
  background: ${({ $danger }) => ($danger ? 'var(--thorium-danger-bg)' : 'var(--thorium-secondary-panel-bg)')};
  color: ${({ $danger }) => ($danger ? 'var(--thorium-button-text)' : 'var(--thorium-secondary-text)')};
`;

/** A small solid dot flagging a node that carries danger-classified tags. */
export const DangerDot = styled.span`
  flex: 0 0 auto;
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--thorium-danger-bg);
`;

export const Spacer = styled.span`
  margin-left: auto;
`;

// --- metadata (condensed under the header, inside the info box) ---

/** The metadata region under the header: a subtle divider + secondary background, tight vertically. */
export const MetadataSection = styled.div`
  border-top: 1px solid var(--thorium-panel-border);
  background: var(--thorium-secondary-panel-bg);
  padding: 0 12px;
`;

/** The condensed "details" caret row — minimal vertical footprint when collapsed. */
export const MetadataToggleRow = styled.div`
  display: flex;
  justify-content: center;
`;

/** The revealed metadata body (only rendered when the details caret is expanded). */
export const MetadataContent = styled.div`
  padding: 2px 0 6px;
`;

export const EmptyNote = styled.div`
  padding: 4px 10px;
  font-size: 0.82rem;
  font-style: italic;
  color: var(--thorium-secondary-text);
`;

export const ShowMoreRow = styled.div`
  display: flex;
  justify-content: center;
  padding: 2px 0;
`;

export const ShowMoreButton = styled.button`
  background: transparent;
  border: none;
  color: var(--thorium-highlight-text);
  font-size: 0.82rem;
  font-weight: 600;
  cursor: pointer;
  padding: 4px 12px;
  border-radius: 6px;

  &:hover {
    background: var(--thorium-highlight-panel-bg);
    color: var(--thorium-text);
  }
`;
