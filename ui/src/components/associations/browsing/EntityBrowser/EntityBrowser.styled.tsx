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
  /* establish a size container so nested Level indentation adapts to the tile's OWN width (narrow dashboard
     column vs. full-width tab vs. expanded) rather than the viewport — see the container queries on Level */
  container-type: inline-size;
  container-name: entitybrowser;
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

/** Slot that holds the hover/focus-revealed hide affordance inside the header's trailing rail. */
export const HideSlot = styled.span`
  flex: 0 0 auto;
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

/** Hover/focus-revealed "focus this subtree" (re-root) affordance in a row header. Mirrors {@link HideButton}. */
export const FocusButton = styled.button`
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

/**
 * The always-visible depth/focus pill shown once a row's nesting passes {@link INDENT_CAP} (where indentation
 * freezes and no longer conveys depth). It reports the depth **and** acts as the re-root trigger — clicking it
 * focuses the tree on that subtree — so the automatic indent cap and the user's focus action are one affordance.
 */
export const DepthPill = styled.button`
  flex: 0 0 auto;
  display: inline-flex;
  align-items: center;
  gap: 3px;
  font-size: 0.68rem;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
  padding: 0 6px;
  height: 1.1rem;
  border-radius: 8px;
  background: var(--thorium-secondary-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  color: var(--thorium-secondary-text);
  cursor: pointer;

  &:hover {
    border-color: var(--thorium-highlight-panel-border);
    color: var(--thorium-text);
  }
  &:focus-visible {
    outline: 2px solid var(--thorium-highlight-text);
    outline-offset: -2px;
  }
`;

/**
 * The focus breadcrumb bar shown above the tree while re-rooted: a clickable trail from "All" (clears the
 * focus) down through the ancestors to the current focus root, so the user can pop back out one level at a
 * time. Wraps on narrow tiles.
 */
export const FocusBar = styled.div`
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 4px;
  padding: 6px 10px;
  font-size: 0.8rem;
  background: var(--thorium-secondary-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
`;

/** A clickable crumb in the {@link FocusBar} (re-roots at that ancestor, or clears focus for the "All" crumb). */
export const Crumb = styled.button`
  display: inline-flex;
  align-items: center;
  gap: 4px;
  max-width: 14rem;
  padding: 1px 6px;
  background: transparent;
  border: none;
  border-radius: 6px;
  color: var(--thorium-link-text);
  font: inherit;
  cursor: pointer;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;

  &:hover {
    background: var(--thorium-highlight-panel-bg);
    color: var(--thorium-highlight-text);
  }
  &:focus-visible {
    outline: 2px solid var(--thorium-highlight-text);
    outline-offset: -2px;
  }
`;

/** The current (last) crumb in the {@link FocusBar}: the focus root itself, shown bold and non-interactive. */
export const CurrentCrumb = styled.span`
  max-width: 18rem;
  padding: 1px 6px;
  font-weight: 700;
  color: var(--thorium-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
`;

/** The `›` separator between focus crumbs. */
export const CrumbSep = styled.span`
  color: var(--thorium-secondary-text);
`;

// --- tree levels / rows ---

/**
 * The nesting depth past which per-level indentation is **frozen**: levels deeper than this add only a
 * hairline {@link FROZEN_INDENT} step (keeping the guide-rule visible) instead of a full indent step, so an
 * arbitrarily deep tree can never march its rows off the right edge. Exported so the row can surface the
 * depth/focus affordance (re-root) at exactly the depth where indentation stops conveying nesting.
 */
export const INDENT_CAP = 6;

/** The minimal per-level indent applied past {@link INDENT_CAP} — just enough to keep nested guide-rules apart. */
const FROZEN_INDENT = '4px';

/**
 * The per-level indent for a level at `$depth`: nothing at the root, one adaptive `--indent-step` for the
 * first {@link INDENT_CAP} levels, then a frozen hairline step. The step itself is set by the container
 * queries below off the browser tile's own width, so a narrow column compresses the indent automatically.
 */
function levelIndent($depth: number): string {
  if ($depth <= 0) return '0';
  return $depth <= INDENT_CAP ? 'var(--indent-step)' : FROZEN_INDENT;
}

/** A nested level; the left guide-rule keeps deep DAGs readable. */
export const Level = styled.div<{ $depth: number }>`
  display: flex;
  flex-direction: column;
  /* very small margin between listed entities to keep the tree compact */
  gap: 2px;
  /* per-level indent step, adapted to the browser tile's own width (container queries below) and frozen past
     INDENT_CAP so deep rows keep usable header width; the guide-rule is retained at every non-root level */
  --indent-step: 10px;
  @container entitybrowser (max-width: 480px) {
    & {
      --indent-step: 6px;
    }
  }
  @container entitybrowser (min-width: 900px) {
    & {
      --indent-step: 14px;
    }
  }
  margin-left: ${({ $depth }) => levelIndent($depth)};
  padding-left: ${({ $depth }) => levelIndent($depth)};
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

/**
 * The row header row: a fixed {@link Chevron} rail, a growing {@link HeaderLead} (identity + badges that
 * wrap as a unit), and a fixed {@link HeaderTrail} (spinner + hide). `align-items: flex-start` keeps the
 * chevron and trailing rail aligned to the FIRST line even when the lead wraps its badges onto later lines.
 */
export const RowHeader = styled.div`
  display: flex;
  align-items: flex-start;
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
  /* reveal the hide + focus affordances when the row is hovered or anything inside it has keyboard focus */
  &:hover ${HideButton}, &:focus-within ${HideButton}, &:hover ${FocusButton}, &:focus-within ${FocusButton} {
    opacity: 1;
  }
`;

/** The shared height of the header's first line, so the chevron / trailing rail center-align to the name row. */
const HEADER_LINE = '1.4rem';

export const Chevron = styled.span<{ $expanded: boolean; $hidden?: boolean }>`
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 12px;
  height: ${HEADER_LINE};
  flex: 0 0 auto;
  color: var(--thorium-secondary-text);
  visibility: ${({ $hidden }) => ($hidden ? 'hidden' : 'visible')};
  transform: rotate(${({ $expanded }) => ($expanded ? '90deg' : '0deg')});
  transition: transform 0.15s ease;
`;

/**
 * The header's growing region: the identity (icon + name) followed by the structural badges and tag chips.
 * A wrapping flex row so, as the row narrows, the whole badge/tag cluster **floats onto the next line under
 * the name** rather than crushing the name — the name keeps the first line, badges/tags flow after it. The
 * row/column gaps are tight (2px vertical, 6px horizontal) so wrapped lines stay compact.
 */
export const HeaderLead = styled.div`
  flex: 1 1 auto;
  min-width: 0;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 2px 6px;
`;

/**
 * The identity cluster (type icon + name) kept together as one wrap-unit so the icon never detaches from the
 * name. `flex: 0 1 auto` lets it take only the width its (ellipsized) name needs — no longer *growing* to eat
 * the row and stranding the badges far to the right (the previous behavior); `min-width: 0` lets the name
 * ellipsize under pressure.
 */
export const HeaderIdentity = styled.span`
  flex: 0 1 auto;
  min-width: 0;
  min-height: ${HEADER_LINE};
  display: inline-flex;
  align-items: center;
  gap: 6px;
`;

/** The header's fixed trailing rail (grow spinner + hide affordance), aligned to the first line. */
export const HeaderTrail = styled.span`
  flex: 0 0 auto;
  min-height: ${HEADER_LINE};
  display: inline-flex;
  align-items: center;
  gap: 6px;
`;

/* Shared name styling: the name takes only the width it needs (no flex-grow), ellipsizing under pressure with
   a small floor so badges/tags sit right after it and wrap beneath it rather than the name eating the row. */
const identifierBase = css`
  flex: 0 1 auto;
  min-width: 0;
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

/**
 * Groups the header's descriptive tag chips. Sits after {@link BadgeGroup} in {@link HeaderLead} and wraps
 * with it, so tags flow right after the structural badges and drop to the next line together under pressure.
 */
export const TagBadgeGroup = styled.span`
  display: inline-flex;
  align-items: center;
  flex-wrap: wrap;
  gap: ${BUTTON_BAR_GAP};
  min-width: 0;
`;

/**
 * A single descriptive tag chip (`key: value`) in the row header. Deliberately lighter than the structural
 * {@link BaseBadge} (dashed, transparent) so tags read as metadata rather than competing with the kind /
 * relationship badges. Capped in width and ellipsized so one long value can't stretch the header.
 */
export const TagBadge = styled.span`
  flex: 0 1 auto;
  min-width: 0;
  max-width: 16rem;
  display: inline-flex;
  align-items: baseline;
  gap: 3px;
  font-size: 0.7rem;
  font-weight: 500;
  padding: 1px 6px;
  border-radius: 10px;
  border: 1px dashed var(--thorium-panel-border);
  color: var(--thorium-secondary-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
`;

/** The key portion of a {@link TagBadge}, de-emphasized so the value stands out. */
export const TagBadgeKey = styled.span`
  flex: 0 0 auto;
  opacity: 0.75;
`;

/** The value portion of a {@link TagBadge}; ellipsizes when the value is long. */
export const TagBadgeValue = styled.span`
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--thorium-text);
`;

/** The `+N` chip shown when a node has more tags than the header cap; its title lists the overflowed tags. */
export const TagOverflowBadge = styled.span`
  flex: 0 0 auto;
  font-size: 0.7rem;
  font-weight: 600;
  padding: 1px 6px;
  border-radius: 10px;
  background: var(--thorium-secondary-panel-bg);
  color: var(--thorium-secondary-text);
  cursor: default;
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
