// project imports
import { StatusUpdate } from '@models/reactions';
import { humanize } from '@utilities/humanize';

/** What a status update's message text holds. */
export enum MessageKind {
  /** No message */
  Empty = 'Empty',
  /** A JSON object */
  Object = 'Object',
  /** A JSON array */
  Array = 'Array',
  /** Text that is not JSON (or a JSON string or number), shown as it is */
  Text = 'Text',
}

/** A status update's message, parsed. */
export interface ParsedMessage {
  /** What the message holds */
  kind: MessageKind;
  /** The parsed JSON value, or the text */
  value: unknown;
}

/**
 * Parse a status update's message, which may be JSON or plain text.
 *
 * @param text - The message, or `null` when the update has none.
 * @returns The parsed message; text that does not parse as a JSON object or array is kept as text.
 */
export function parseMessage(text: string | null | undefined): ParsedMessage {
  const trimmed = (text ?? '').trim();
  if (trimmed === '') return { kind: MessageKind.Empty, value: '' };
  if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
    try {
      const value: unknown = JSON.parse(trimmed);
      if (Array.isArray(value)) return { kind: MessageKind.Array, value };
      if (value !== null && typeof value === 'object') return { kind: MessageKind.Object, value };
    } catch {
      // text that only looks like JSON is shown as it is
    }
  }
  return { kind: MessageKind.Text, value: text ?? '' };
}

/** The update fields that repeat what the run's own page already shows. */
export const HIDDEN_UPDATE_KEYS = ['group', 'pipeline', 'reaction', 'id', 'current_stage'];

/** The fields a reader looks for first, in the order they show, with their labels. */
const KNOWN_FIELDS: [string, string][] = [
  ['status', 'Status'],
  ['stage', 'Stage'],
  ['tool', 'Tool'],
  ['tool_instance', 'Tool instance'],
  ['job', 'Job'],
  ['worker', 'Worker'],
  ['error', 'Error'],
  ['reason', 'Reason'],
  ['message', 'Message'],
  ['count', 'Count'],
  ['progress', 'Progress'],
  ['completed', 'Completed'],
  ['total', 'Total'],
  ['deadline', 'Deadline'],
  ['sla', 'SLA'],
];

/** The keys whose values are error text, drawn in the error style. */
export const ERROR_KEYS = ['error', 'reason'];

/** One labelled field of a status update. */
export interface MessageField {
  /** The field's key */
  key: string;
  /** The field's label */
  label: string;
  /** The field's value: text, a number, or a nested object or array */
  value: unknown;
}

/**
 * Read a status update field, which the API stores JSON encoded for some values.
 *
 * @param value - The stored text.
 * @returns The value with one layer of JSON string quoting removed.
 */
export function updateValue(value: string): string {
  // a status is stored as the JSON of its enum, so it arrives wrapped in quotes
  if (value.length >= 2 && value.startsWith('"') && value.endsWith('"')) {
    try {
      const parsed: unknown = JSON.parse(value);
      if (typeof parsed === 'string') return parsed;
    } catch {
      // text that only looks quoted is shown as it is
    }
  }
  return value;
}

/**
 * Label a field key: a known key gets its fixed label, any other key is humanised.
 *
 * @param key - The key.
 * @returns The label.
 */
export function fieldLabel(key: string): string {
  return KNOWN_FIELDS.find(([known]) => known === key)?.[1] ?? humanize(key);
}

/**
 * Turn the fields of a status update into labelled fields: the update's own fields, and the fields of a
 * JSON object message, with the known fields first in a fixed order and the rest by key.
 *
 * @param update - The status update.
 * @returns The labelled fields, without the ones the page already shows.
 */
export function messageFields(update: Pick<StatusUpdate, 'msg' | 'update'>): MessageField[] {
  const values = new Map<string, unknown>();
  for (const [key, value] of Object.entries(update.update ?? {})) {
    if (!HIDDEN_UPDATE_KEYS.includes(key)) values.set(key, updateValue(String(value)));
  }
  const message = parseMessage(update.msg);
  if (message.kind === MessageKind.Object) {
    for (const [key, value] of Object.entries(message.value as Record<string, unknown>)) values.set(key, value);
  }
  const known = KNOWN_FIELDS.map(([key]) => key).filter((key) => values.has(key));
  const rest = [...values.keys()].filter((key) => !known.includes(key)).sort();
  return [...known, ...rest].map((key) => ({ key, label: fieldLabel(key), value: values.get(key) }));
}

/**
 * Pretty-print a status update's message and fields as JSON with a two space indent.
 *
 * @param update - The status update.
 * @returns The JSON text: the parsed message (or its text) and the update's fields with their quoting removed.
 */
export function rawMessage(update: Pick<StatusUpdate, 'msg' | 'update'>): string {
  const message = parseMessage(update.msg);
  const fields = Object.fromEntries(Object.entries(update.update ?? {}).map(([key, value]) => [key, updateValue(String(value))]));
  const raw: Record<string, unknown> = {};
  if (message.kind !== MessageKind.Empty) raw.msg = message.value;
  raw.update = fields;
  return JSON.stringify(raw, null, 2);
}

/** The most characters of one text value shown before a Show more control. */
export const TEXT_LIMIT = 300;

/** The most entries of one object or array shown before a Show more control. */
export const ENTRY_LIMIT = 20;

/** How deep nested objects show open; deeper ones start collapsed. */
export const OPEN_DEPTH = 2;

/**
 * Cut a long text for display.
 *
 * @param text - The text.
 * @param limit - The most characters to show.
 * @returns The shown part and how many characters were left out.
 */
export function truncateText(text: string, limit = TEXT_LIMIT): { shown: string; hidden: number } {
  return text.length <= limit ? { shown: text, hidden: 0 } : { shown: text.slice(0, limit), hidden: text.length - limit };
}

/**
 * List the entries of an object or array for display.
 *
 * @param value - The object or array.
 * @returns The entries as key and value pairs; array entries are keyed by their position.
 */
export function valueEntries(value: object): [string, unknown][] {
  return Array.isArray(value) ? value.map((item, index) => [String(index), item]) : Object.entries(value);
}

/**
 * Tell whether a value nests other values.
 *
 * @param value - The value.
 * @returns Whether it is a non-null object or an array.
 */
export function isNested(value: unknown): value is object {
  return value !== null && typeof value === 'object';
}

/**
 * Turn a scalar into display text.
 *
 * @param value - A string, number, boolean, or null.
 * @returns The text; null shows as `null`.
 */
export function scalarText(value: unknown): string {
  if (value === null || value === undefined) return 'null';
  return typeof value === 'string' ? value : JSON.stringify(value);
}
