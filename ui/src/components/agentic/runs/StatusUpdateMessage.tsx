import { FC, useState } from 'react';
import styled from 'styled-components';

// project imports
import { RunTime } from './RunTime';
import { StatusPill } from './StatusIndicator';
import {
  ENTRY_LIMIT,
  ERROR_KEYS,
  isNested,
  MessageKind,
  messageFields,
  OPEN_DEPTH,
  parseMessage,
  rawMessage,
  scalarText,
  truncateText,
  valueEntries,
} from './updateMessage';
import { Muted, Pre } from '@components/agentic/shared';
import { Button, ButtonSize, ButtonVariant } from '@components/shared/buttons';
import { StatusUpdate } from '@models/reactions';

/** The keys whose values are timestamps, drawn as relative times. */
const TIME_KEYS = ['deadline', 'sla', 'timestamp'];

/** A message's blocks, stacked with a small gap. */
const MessageBlock = styled.div`
  display: flex;
  flex-direction: column;
  gap: 0.25rem;
  min-width: 0;
  overflow-wrap: anywhere;
`;

/** The labelled fields of a message on wrapping lines. */
const FieldList = styled.div`
  display: flex;
  flex-wrap: wrap;
  gap: 0.25rem 1rem;
`;

/** One labelled field. */
const FieldItem = styled.span`
  min-width: 0;
`;

/** A field's label. */
const FieldLabel = styled.span`
  color: var(--thorium-secondary-text);
  font-weight: 600;
  margin-right: 0.25rem;
`;

/** Error text from a message, drawn in the error colour. */
const ErrorText = styled.span`
  color: var(--thorium-danger-bg);
  font-weight: 600;
`;

/** A nested object or array, indented under its key. */
const Tree = styled.dl`
  margin: 0;
  padding-left: 1rem;
  border-left: 2px solid var(--thorium-panel-border);

  dt {
    display: inline;
    color: var(--thorium-secondary-text);
    font-weight: 600;
    margin-right: 0.25rem;
  }

  dd {
    display: inline;
    margin: 0;
  }

  dd::after {
    content: '';
    display: block;
  }
`;

/** A collapsed nested value's summary line. */
const Summary = styled.summary`
  cursor: pointer;
  color: var(--thorium-secondary-text);
`;

/** Props for {@link LongText}. */
interface LongTextProps {
  /** The text */
  text: string;
}

/** Text that wraps, cut after a limit with a Show more control. */
const LongText: FC<LongTextProps> = ({ text }) => {
  const [all, setAll] = useState(false);
  const { shown, hidden } = truncateText(text);
  if (hidden === 0 || all) return <span>{text}</span>;
  return (
    <span>
      {shown}…{' '}
      <Button size={ButtonSize.XSmall} variant={ButtonVariant.Ghost} data-testid="update-show-more" onClick={() => setAll(true)}>
        Show {hidden} more characters
      </Button>
    </span>
  );
};

/** Props for {@link ValueTree}. */
interface ValueTreeProps {
  /** The value */
  value: unknown;
  /** How deep the value is nested */
  depth: number;
}

/** A value of any shape: text, or a nested object or array as an indented key and value list, collapsed past a depth. */
export const ValueTree: FC<ValueTreeProps> = ({ value, depth }) => {
  const [all, setAll] = useState(false);
  if (!isNested(value)) return <LongText text={scalarText(value)} />;
  const entries = valueEntries(value);
  if (entries.length === 0) return <Muted>{Array.isArray(value) ? 'none' : 'empty'}</Muted>;
  const shown = all ? entries : entries.slice(0, ENTRY_LIMIT);
  const tree = (
    <Tree>
      {shown.map(([key, item]) => (
        <div key={key}>
          <dt>{key}</dt>
          <dd>
            <ValueTree value={item} depth={depth + 1} />
          </dd>
        </div>
      ))}
      {entries.length > shown.length && (
        <Button size={ButtonSize.XSmall} variant={ButtonVariant.Ghost} data-testid="update-show-more-entries" onClick={() => setAll(true)}>
          Show {entries.length - shown.length} more
        </Button>
      )}
    </Tree>
  );
  if (depth < OPEN_DEPTH) return tree;
  // deep values start collapsed so one large payload doesn't push the rest of the updates away
  return (
    <details>
      <Summary>
        {entries.length} {Array.isArray(value) ? 'items' : 'keys'}
      </Summary>
      {tree}
    </details>
  );
};

/** Props for {@link FieldValue}. */
interface FieldValueProps {
  /** The field's key */
  name: string;
  /** The field's value */
  value: unknown;
}

/** One field's value: a status as a pill, a time as a relative time, error text in the error colour, and anything else as a value tree. */
const FieldValue: FC<FieldValueProps> = ({ name, value }) => {
  if (name === 'status' && typeof value === 'string') return <StatusPill status={value} />;
  if (TIME_KEYS.includes(name) && typeof value === 'string') return <RunTime value={value} />;
  if (ERROR_KEYS.includes(name) && typeof value === 'string') {
    return (
      <ErrorText>
        <LongText text={value} />
      </ErrorText>
    );
  }
  return <ValueTree value={value} depth={1} />;
};

/** Props for {@link StatusUpdateMessage}. */
interface StatusUpdateMessageProps {
  /** The status update */
  update: Pick<StatusUpdate, 'msg' | 'update'>;
}

/**
 * A status update's message and fields, readable: plain text as it is, the known fields labelled, nested
 * values as indented lists, and a Raw toggle with the pretty-printed JSON. Every value is rendered as text.
 */
export const StatusUpdateMessage: FC<StatusUpdateMessageProps> = ({ update }) => {
  const [raw, setRaw] = useState(false);
  const message = parseMessage(update.msg);
  const fields = messageFields(update);
  return (
    <MessageBlock data-testid="update-message">
      {message.kind === MessageKind.Text && (
        <span data-testid="update-text">
          <LongText text={String(message.value)} />
        </span>
      )}
      {message.kind === MessageKind.Array && <ValueTree value={message.value} depth={0} />}
      <FieldList>
        {fields.map((field) => (
          <FieldItem key={field.key} data-testid={`update-field-${field.key}`}>
            <FieldLabel>{field.label}</FieldLabel>
            <FieldValue name={field.key} value={field.value} />
          </FieldItem>
        ))}
        {/* the toggle ends the fields' line instead of taking a line of its own */}
        <FieldItem>
          <Button size={ButtonSize.XSmall} variant={ButtonVariant.Ghost} data-testid="update-raw-toggle" onClick={() => setRaw(!raw)}>
            {raw ? 'Hide raw' : 'Raw'}
          </Button>
        </FieldItem>
      </FieldList>
      {raw && <Pre data-testid="update-raw">{rawMessage(update)}</Pre>}
    </MessageBlock>
  );
};
