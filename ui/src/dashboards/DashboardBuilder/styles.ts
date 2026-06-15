// spec: ./SPEC.md

import styled from 'styled-components';

// project imports
import { spacers } from '@styles';

/**
 * The builder page grid: stacked full-width rows — selected-resources tile, the depth/create menu,
 * then the browse list (which carries its own resource-type dropdown). Everything is a single
 * column; the page is an authoring surface rather than a data dashboard, so no responsive column
 * split is needed.
 */
export const BuilderLayout = styled.div`
  display: flex;
  flex-direction: column;
  gap: ${spacers.four};
  padding-bottom: ${spacers.five};
`;

/**
 * The pinned summary tile at the top of the builder holding the selected-resources chips in a themed
 * panel. A flex column so future summary rows stack with a consistent gap.
 */
export const SummaryTile = styled.section`
  display: flex;
  flex-direction: column;
  gap: ${spacers.four};
  background: var(--thorium-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  padding: ${spacers.four};
  min-width: 0;
`;

/**
 * The depth/create menu row: the depth control and the Create button centered together with no
 * card/background fill, ordered Depth then Create. A borderless flex row so it reads as a light
 * action bar rather than a panel; wraps on narrow viewports.
 */
export const BuilderMenu = styled.div`
  display: flex;
  align-items: center;
  justify-content: center;
  gap: ${spacers.three};
  flex-wrap: wrap;
`;

/**
 * A titled section wrapper (type picker, browse list, selection panel) with a themed panel look.
 *
 * The trailing `EntityList` pagination (a react-bootstrap `<Pagination>` in a `Row.mt-3`) carries its own
 * bottom margin, which leaves Back/Next floating too far above the card's bottom edge. Zeroing the
 * pagination's bottom margin here — scoped to the builder so other browse pages are unaffected — lets it
 * sit a normal gap above the card edge.
 */
export const BuilderSection = styled.section`
  background: var(--thorium-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  padding: ${spacers.four};
  min-width: 0;

  & .pagination {
    margin-bottom: 0;
  }
`;

/// A small uppercase section label shown above a section's content.
export const SectionLabel = styled.h3`
  margin: 0 0 ${spacers.three} 0;
  font-size: 0.8rem;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--thorium-secondary-text);
`;

/**
 * The controls row placing the resource-type picker inline, directly in front of the filters omnibar (or
 * the Tag-mode key/value entry). The picker keeps its own (compact) width while the LAST child — a fill
 * wrapper around the omnibar/tags-select — flexes to fill the rest of the row, so the row reads as
 * `[picker][filters]`. A small bottom margin separates it from the list below.
 */
export const BrowseControls = styled.div`
  display: flex;
  align-items: center;
  gap: ${spacers.three};
  margin-bottom: ${spacers.two};
  min-width: 0;

  & > *:last-child {
    flex: 1;
    min-width: 0;
  }
`;

/// Groups the depth label and its selector in the footer.
export const DepthGroup = styled.div`
  display: flex;
  align-items: center;
  gap: ${spacers.three};
  color: var(--thorium-text);
  font-size: 0.9rem;
`;

/**
 * The custom "Create dashboard" button.
 *
 * A styled-component (not the legacy react-bootstrap `<Button>`) so the builder stays react-bootstrap
 * free. Disabled styling dims the button and blocks pointer events; the caller wraps it in a
 * focusable span so the overlay tip still fires while disabled.
 */
export const CreateButton = styled.button`
  display: inline-flex;
  align-items: center;
  gap: ${spacers.three};
  padding: ${spacers.three} ${spacers.five};
  border: 1px solid var(--thorium-highlight-panel-border);
  border-radius: 8px;
  background: var(--thorium-ok-bg);
  color: var(--thorium-button-text);
  font-size: 0.9rem;
  font-weight: 600;
  cursor: pointer;

  &:hover:not(:disabled) {
    filter: brightness(1.1);
  }

  &:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
`;

/**
 * A single browse row: the rendered entity on the left, the add/added control on the right.
 */
export const BrowseRow = styled.div`
  display: flex;
  align-items: center;
  gap: ${spacers.three};
  min-width: 0;

  & > *:first-child {
    flex: 1;
    min-width: 0;
  }
`;

/**
 * The per-row add/remove control. Rendered as a square icon button (`$square`) so the icon sits
 * centered in a square rather than a wide pill. `$remove` styles the already-added state as an
 * actionable "remove" (danger-tinted hover) carrying an X, versus the default add (plus) state.
 */
export const AddButton = styled.button<{ $remove?: boolean; $square?: boolean }>`
  flex: 0 0 auto;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  ${({ $square }) => ($square ? 'width: 1.75rem; height: 1.75rem; padding: 0;' : `padding: ${spacers.two} ${spacers.four};`)}
  border: 1px solid var(--thorium-panel-border);
  border-radius: 6px;
  font-size: 0.8rem;
  font-weight: 600;
  white-space: nowrap;
  background: var(--thorium-highlight-panel-bg);
  color: var(--thorium-text);
  cursor: pointer;

  &:hover:not(:disabled) {
    filter: brightness(1.1);
    ${({ $remove }) => ($remove ? 'background: var(--thorium-danger-bg); color: var(--thorium-button-text);' : '')}
  }

  &:disabled {
    cursor: default;
  }
`;

/// The key/value entry row used in Tag mode (a TagSelect plus an Add button), vertically centered so
/// the add button lines up with the tag input.
export const TagModeRow = styled.div`
  display: flex;
  align-items: center;
  gap: ${spacers.three};
  min-width: 0;

  & > *:first-child {
    flex: 1;
    min-width: 0;
  }
`;

/// A muted placeholder shown when a browse subsection has no results.
export const EmptyBrowse = styled.div`
  padding: ${spacers.four};
  text-align: center;
  color: var(--thorium-secondary-text);
  font-size: 0.85rem;
`;
