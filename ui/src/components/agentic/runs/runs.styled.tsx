import styled from 'styled-components';

// project imports
import { Table, WideTable } from '@components/agentic/shared';

/** A table inside a run card: rows are separated from each other, and the last row draws no line above the card's edge. */
export const CardTable = styled(Table)`
  tbody tr:last-child td {
    border-bottom: none;
  }
`;

/** A {@link CardTable} that keeps a minimum width; wrap it in a TableScroll. */
export const CardWideTable = styled(WideTable)`
  tbody tr:last-child td {
    border-bottom: none;
  }
`;

/** A list of blocks in a run card, separated by a line between neighbours only, so the card never ends with a line. */
export const SeparatedList = styled.div`
  & > * + * {
    margin-top: 0.75rem;
    padding-top: 0.75rem;
    border-top: 1px solid var(--thorium-panel-border);
  }
`;

/** A run of controls and labels on one line that wraps when it runs out of room. */
export const InlineRow = styled.div`
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.25rem 0.5rem;
`;
