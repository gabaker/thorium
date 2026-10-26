import styled from 'styled-components';

// project imports
import { Table } from '@components/agentic/shared';
import { BUTTON_BADGE_GAP } from '@styles';

/// The bordered panel that holds the scoped token tables
export const ScopedPanel = styled.section`
  margin: 1rem 0;
  padding: 1rem 0.75rem;
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  background-color: var(--thorium-panel-bg);
  color: var(--thorium-text);
`;

/// The header of a panel or subsection: the title and description on the left, actions on the right
export const PanelHead = styled.div`
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 0.5rem 1rem;
  padding-bottom: 0.75rem;
  border-bottom: 1px solid var(--thorium-panel-border);
`;

/// The title and the description under it
export const PanelHeading = styled.div`
  min-width: 0;
`;

/// The explanatory line under a title
export const PanelDescription = styled.p`
  margin: 0.25rem 0 0 0;
  color: var(--thorium-secondary-text);
  font-size: 0.85rem;
`;

/// A subsection of the panel, set apart from the table above it
export const PanelSubsection = styled.div`
  margin-top: 1rem;
`;

/// A line of status text (loading, empty) shown where a table would be
export const PanelNotice = styled.p`
  margin: 0;
  padding: 1rem 0 0.25rem 0;
  color: var(--thorium-secondary-text);
`;

/// Scrolls a table sideways rather than squeezing its columns when the window is narrow; dims while
/// the list reloads so the previous rows stay readable
export const TableFrame = styled.div<{ $busy?: boolean }>`
  overflow-x: auto;
  opacity: ${({ $busy }) => ($busy ? 0.6 : 1)};
  transition: opacity 0.15s ease;
`;

/// A token table; the column widths follow the content, headers and controls wrap before the table
/// outgrows a narrow panel, and the frame scrolls when the columns still cannot fit
export const TokenTable = styled(Table)`
  min-width: 40rem;

  th,
  td {
    vertical-align: middle;
  }

  th {
    white-space: normal;
  }

  /* the token being used stands out from the other rows */
  tr[data-active='true'] {
    background: var(--thorium-secondary-panel-bg);
  }
`;

/// The name of a token with an optional second line under it; a long name with no separators breaks
/// anywhere, which (unlike `break-word`) also lowers the column's minimum width
export const NameCell = styled.td`
  min-width: 6rem;
  font-weight: 600;
  overflow-wrap: anywhere;
`;

/// A cell that never wraps, for dates and short controls
export const NoWrapCell = styled.td`
  white-space: nowrap;
`;

/// The masked token value; monospace so its width matches the value it hides
export const MaskedToken = styled.span`
  font-family: ui-monospace, SFMono-Regular, SFMono, monospace;
  color: var(--thorium-secondary-text);
`;

/// The copy control and masked value of a token
export const TokenValue = styled.div`
  display: flex;
  align-items: center;
  gap: 0.25rem;
  white-space: nowrap;
`;

/// The row of buttons that act on a token
export const RowActions = styled.div`
  display: flex;
  flex-wrap: wrap;
  justify-content: flex-end;
  gap: 0.4rem;
`;

/// The header cell over the row actions; stays at the right edge while a narrow table scrolls
export const ActionsHeader = styled.th`
  position: sticky;
  right: 0;
  text-align: right;
  background: var(--thorium-panel-bg);
`;

/// The cell holding the row actions; stays at the right edge while a narrow table scrolls so the
/// controls never scroll out of reach
export const ActionsCell = styled.td`
  position: sticky;
  right: 0;
  background: var(--thorium-panel-bg);
`;

/// The cell holding a token's group pills; its minimum width fits a name of about ten characters so
/// the auto table layout never squeezes an ordinary group name into a mid-word break, while a longer
/// name still breaks inside its pill
export const GroupsCell = styled.td`
  min-width: 6.5rem;
`;

/// The groups of a token; pills wrap onto further lines rather than widening the table
export const GroupPills = styled.div`
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: ${BUTTON_BADGE_GAP};
`;

/// Muted text such as a placeholder value or the second line of a cell
export const CellNote = styled.div`
  color: var(--thorium-secondary-text);
  font-size: 0.8rem;
  font-weight: 400;
`;
