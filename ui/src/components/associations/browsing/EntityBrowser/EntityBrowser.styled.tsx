import styled, { keyframes } from 'styled-components';

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

// --- tree levels / rows ---

/** A nested level; the left guide-rule keeps deep DAGs readable. */
export const Level = styled.div<{ $depth: number }>`
  display: flex;
  flex-direction: column;
  /* very small margin between listed entities to keep the tree compact */
  gap: 2px;
  margin-left: ${({ $depth }) => ($depth > 0 ? 10 : 0)}px;
  padding-left: ${({ $depth }) => ($depth > 0 ? 10 : 0)}px;
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

export const IdentifierLink = styled.a`
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 600;
  color: var(--thorium-link-text);
  text-decoration: none;

  &:hover {
    color: var(--thorium-highlight-text);
  }
`;

export const IdentifierText = styled.span`
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 600;
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
  // "… In <container> <Kind>" labels can get long — cap and ellipsize (full text in the title attr)
  max-width: 22rem;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
`;

export const ViaBadge = styled(BaseBadge)`
  background: transparent;
  border: 1px dashed var(--thorium-panel-border);
  font-style: italic;
  font-weight: 500;
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
