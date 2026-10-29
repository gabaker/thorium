import { FC, MouseEvent, ReactNode } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import styled from 'styled-components';

// project imports
import { Severity } from '@components/shared/alerts/AlertBanner';

// the pill primitives live in the shared badges module; agentic pages keep importing them from here
export { Badge, BadgeSize, BadgeTone, statusTone, WrapBadge } from '@components/shared/badges/Badge';
// the inline code primitives live in the shared syntax module, so shared banners can render API text with them too
export { CodeSpans, Mono } from '@components/shared/syntax/CodeSpans';

/** A wrapping row of badges or links; the gap spaces the items, so a wrapped item starts flush with the first. */
export const InlineList = styled.div`
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.25rem 0.5rem;
`;

/** The header row of a page: title on the left, actions on the right. */
export const PageHead = styled.div`
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  flex-wrap: wrap;
  margin-bottom: 1rem;
  /* the height of a Medium button, so the title sits at the same y on pages with and without header actions */
  min-height: 2.125rem;
`;

/** The page title. */
export const PageTitle = styled.h1`
  font-size: 1.5rem;
  text-transform: uppercase;
  margin: 0;
  /* a long unbroken name (an entity or stage name) wraps inside the page instead of widening it */
  overflow-wrap: anywhere;
`;

/** A row of buttons. */
export const Actions = styled.div`
  display: flex;
  gap: 0.5rem;
  flex-wrap: wrap;
  align-items: center;
`;

/** A bordered block with spacing, used for each section of a page. */
export const Section = styled.section`
  background: var(--thorium-panel-bg);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 8px;
  padding: 1rem;
  margin-bottom: 1rem;

  h2 {
    font-size: 1.1rem;
    margin: 0 0 0.75rem 0;
  }

  /* a heading that follows a value (a code block or muted text, neither of which has a bottom margin) gets the gap itself */
  * + h2 {
    margin-top: 1rem;
  }

  h3 {
    font-size: 0.95rem;
    font-weight: 600;
    margin: 1rem 0 0.5rem 0;
  }

  /* a card never ends on a separator: the last row of a table that ends it (bare or in a scroll box) drops its rule,
     and a trailing hr is hidden, whichever child happens to render last */
  > :last-child tbody > tr:last-child > td,
  > :last-child tbody > tr:last-child > th {
    border-bottom: none;
  }

  > hr:last-child {
    display: none;
  }
`;

/** A data table with themed borders. */
export const Table = styled.table`
  width: 100%;
  border-collapse: collapse;
  font-size: 0.9rem;

  th,
  td {
    text-align: left;
    padding: 0.4rem 0.6rem;
    border-bottom: 1px solid var(--thorium-panel-border);
    vertical-align: top;
  }

  th {
    color: var(--thorium-secondary-text);
    font-weight: 600;
    white-space: nowrap;
  }

  tbody tr:hover {
    background: var(--thorium-highlight-panel-bg);
  }
`;

/** A data table whose cells share one vertical centre line, for rows that hold a button next to text. */
export const CenteredTable = styled(Table)`
  td {
    vertical-align: middle;
  }
`;

/** A horizontally scrolling box for a table that needs more room than a narrow window gives it. */
export const TableScroll = styled.div`
  overflow-x: auto;
`;

/** A table that keeps a minimum width so its cells don't wrap badly; wrap it in {@link TableScroll}. */
export const WideTable = styled(Table)`
  min-width: 38rem;
`;

/** A run of text that never breaks inside itself, so a hyphenated identifier stays whole in a narrow table cell. */
export const NoWrap = styled.span`
  white-space: nowrap;
`;

/** A name that stays on one line when it is short and otherwise breaks at its hyphens, or anywhere when it has none. */
export const FlexName = styled.span<{ $short: boolean }>`
  ${({ $short }) => ($short ? 'white-space: nowrap;' : 'overflow-wrap: anywhere;')}
`;

/** A two column label and value grid. */
export const KeyValues = styled.dl`
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 0.35rem 1rem;
  margin: 0;

  dt {
    color: var(--thorium-secondary-text);
    font-weight: 600;
  }

  dd {
    margin: 0;
    word-break: break-word;
  }
`;

/** Preformatted text such as specs, logs, and JSON. */
export const Pre = styled.pre`
  background: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 6px;
  padding: 0.75rem;
  margin: 0;
  max-height: 28rem;
  overflow: auto;
  font-size: 0.8rem;
  white-space: pre-wrap;
  word-break: break-word;
`;

/** A labelled form field. */
export const Field = styled.label`
  display: flex;
  flex-direction: column;
  gap: 0.25rem;
  margin-bottom: 0.75rem;

  > span.label {
    font-weight: 600;
    font-size: 0.9rem;
  }

  > span.hint {
    color: var(--thorium-secondary-text);
    font-size: 0.8rem;
  }

  > span.error {
    color: var(--thorium-error-bg);
    font-size: 0.8rem;
    font-weight: 600;
  }
`;

/** A field kept off the field or value above it, for a field that follows a block without a bottom margin. */
export const SpacedField = styled(Field)`
  margin-top: 0.75rem;
`;

/** A multi-line text input. */
export const TextArea = styled.textarea`
  background-color: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 6px;
  padding: 0.5rem 0.75rem;
  font-family: monospace;
  font-size: 0.85rem;
  width: 100%;
  min-height: 8rem;
  resize: vertical;

  &:focus-visible {
    outline: none;
    border-color: var(--thorium-highlight-panel-border);
  }
`;

/** A native select styled like the text inputs. */
export const Select = styled.select`
  background-color: var(--thorium-secondary-panel-bg);
  color: var(--thorium-text);
  border: 1px solid var(--thorium-panel-border);
  border-radius: 6px;
  padding: 0.5rem 0.75rem;
  font-size: 1rem;
  width: 100%;

  &:focus-visible {
    outline: none;
    border-color: var(--thorium-highlight-panel-border);
  }
`;

/** A list of checkboxes laid out in a scrollable box. */
export const CheckList = styled.div`
  display: flex;
  flex-direction: column;
  gap: 0.25rem;
  max-height: 12rem;
  overflow: auto;
  padding: 0.5rem;
  border: 1px solid var(--thorium-panel-border);
  border-radius: 6px;
  background: var(--thorium-secondary-panel-bg);

  label {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin: 0;
    font-weight: normal;
  }

  label.unavailable {
    opacity: 0.6;
  }
`;

/** Muted helper text. */
export const Muted = styled.span`
  color: var(--thorium-secondary-text);
`;

/** Props for {@link ShortId}. */
interface ShortIdProps {
  /** The id of the entity whose name repeats */
  id: string;
  /** Whether another listed entity has the same name; the short id is only shown then */
  show: boolean;
}

/** The first characters of an id, shown beside a name that another listed entity shares so the two can be told apart. */
export const ShortId: FC<ShortIdProps> = ({ id, show }) =>
  show ? (
    <Muted data-testid="short-id" title={id} style={{ marginLeft: '0.4rem' }}>
      #{id.slice(0, 8)}
    </Muted>
  ) : null;

/** A row that wraps its children with a gap. */
export const Row = styled.div`
  display: flex;
  gap: 0.75rem;
  flex-wrap: wrap;
  align-items: center;
`;

/** A native checkbox label row. */
export const CheckRow = styled.label`
  display: flex;
  align-items: center;
  gap: 0.5rem;
  margin: 0;
`;

/** A stack of banners inside a section (warnings, the result of an action), kept off the control or text above it. */
export const BannerStack = styled.div`
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
  margin-top: 0.75rem;
`;

/** Holds a page's error and notice banners and keeps a gap between them and the content below; takes no space when empty. */
export const BannerSlot = styled.div`
  display: flex;
  flex-direction: column;
  gap: 0.5rem;

  &:not(:empty) {
    margin-bottom: 1rem;
  }
`;

/** Props for {@link EntityLink}. */
interface EntityLinkProps {
  /** The route to link to. */
  to: string;
  /** The link text. */
  children: ReactNode;
}

/** A themed in-app link. */
export const EntityLink: FC<EntityLinkProps> = ({ to, children }) => (
  <Link to={to} style={{ color: 'var(--thorium-link-text)' }}>
    {children}
  </Link>
);

/** What a click on a {@link LinkRow} did, as far as deciding whether it opens the row. */
export interface RowClick {
  /** The mouse button; 0 is the primary button. */
  button: number;
  /** Whether a modifier key (meta, ctrl, shift, or alt) was held, which the browser uses for its own link actions. */
  modified: boolean;
  /** Whether the click landed on a link, button, or form control inside the row, which handles the click itself. */
  onControl: boolean;
  /** Whether the click ended a text selection, so the user was copying text rather than opening the row. */
  selecting: boolean;
}

/**
 * Decide whether a click on a row should open the row's page.
 *
 * @param click - The click to judge.
 */
export const opensRow = (click: RowClick): boolean => click.button === 0 && !click.modified && !click.onControl && !click.selecting;

/** Props for {@link LinkRow}. */
interface LinkRowProps {
  /** The route the row opens. */
  to: string;
  /** The test id of the row. */
  testId: string;
  /** The row's cells. */
  children: ReactNode;
}

/** A table row that opens a page when clicked anywhere, matching the hover highlight; its own link and controls still work. */
export const LinkRow: FC<LinkRowProps> = ({ to, testId, children }) => {
  const navigate = useNavigate();
  // open the row's page unless the click was for something inside the row or a text selection
  const onClick = (event: MouseEvent<HTMLTableRowElement>) => {
    const target = event.target as Element;
    const open = opensRow({
      button: event.button,
      modified: event.metaKey || event.ctrlKey || event.shiftKey || event.altKey,
      onControl: target.closest('a, button, input, select, textarea, label') !== null,
      selecting: (window.getSelection()?.toString() ?? '') !== '',
    });
    if (open) void navigate(to);
  };
  return (
    <tr data-testid={testId} style={{ cursor: 'pointer' }} onClick={onClick}>
      {children}
    </tr>
  );
};

/** Map a notification level or ban to a banner severity. */
export const SEVERITY_BY_LEVEL: Record<string, Severity> = {
  Info: Severity.Info,
  Warn: Severity.Warning,
  Error: Severity.Error,
};
