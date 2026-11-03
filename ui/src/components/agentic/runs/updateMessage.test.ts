import { describe, expect, it } from 'vitest';

// project imports
import {
  fieldLabel,
  MessageKind,
  messageFields,
  parseMessage,
  rawMessage,
  TEXT_LIMIT,
  truncateText,
  updateValue,
  valueEntries,
} from './updateMessage';

describe('parseMessage', () => {
  it('parses a JSON object', () => {
    expect(parseMessage('{"status":"Failed","error":"boom"}')).toEqual({
      kind: MessageKind.Object,
      value: { status: 'Failed', error: 'boom' },
    });
  });
  it('parses a JSON array', () => {
    expect(parseMessage(' [1, {"a": 2}] ')).toEqual({ kind: MessageKind.Array, value: [1, { a: 2 }] });
  });
  it('keeps invalid JSON as text', () => {
    expect(parseMessage('{"status": ')).toEqual({ kind: MessageKind.Text, value: '{"status": ' });
  });
  it('keeps plain text as it is', () => {
    expect(parseMessage('the worker was lost')).toEqual({ kind: MessageKind.Text, value: 'the worker was lost' });
  });
  it('has nothing to show for no message', () => {
    expect(parseMessage(null).kind).toBe(MessageKind.Empty);
    expect(parseMessage('   ').kind).toBe(MessageKind.Empty);
  });
});

describe('messageFields', () => {
  it('labels the known fields first in a fixed order, unquotes JSON strings, and drops what the page shows', () => {
    const fields = messageFields({
      msg: null,
      update: {
        zeta: '1',
        group: 'g',
        pipeline: 'p',
        reaction: 'r',
        id: 'i',
        current_stage: '0',
        job: 'j1',
        stage: 'scan',
        status: '"Created"',
      },
    });
    expect(fields.map((field) => [field.label, field.value])).toEqual([
      ['Status', 'Created'],
      ['Stage', 'scan'],
      ['Job', 'j1'],
      ['Zeta', '1'],
    ]);
  });
  it('merges the fields of a JSON object message, nested values kept whole', () => {
    const fields = messageFields({ msg: '{"error":"boom","tool":"yara","counts":{"done":2,"total":5}}', update: { status: 'Failed' } });
    expect(fields.map((field) => field.key)).toEqual(['status', 'tool', 'error', 'counts']);
    expect(fields.find((field) => field.key === 'counts')?.value).toEqual({ done: 2, total: 5 });
  });
  it('humanises an unknown key', () => {
    expect(fieldLabel('worker_count')).toBe('Worker Count');
    expect(fieldLabel('sla')).toBe('SLA');
  });
  it('keeps text that only looks quoted', () => {
    expect(updateValue('"half')).toBe('"half');
  });
});

describe('rawMessage', () => {
  it('pretty-prints the message and fields with a two space indent', () => {
    expect(rawMessage({ msg: '{"a":[1]}', update: { status: '"Created"' } })).toBe(
      '{\n  "msg": {\n    "a": [\n      1\n    ]\n  },\n  "update": {\n    "status": "Created"\n  }\n}',
    );
  });
  it('keeps a plain text message as a string and leaves out an empty one', () => {
    expect(JSON.parse(rawMessage({ msg: 'hello', update: {} }))).toEqual({ msg: 'hello', update: {} });
    expect(JSON.parse(rawMessage({ msg: null, update: { a: 'b' } }))).toEqual({ update: { a: 'b' } });
  });
});

describe('truncation', () => {
  it('cuts a huge payload and counts what it left out', () => {
    const huge = 'x'.repeat(TEXT_LIMIT + 1234);
    expect(truncateText(huge)).toEqual({ shown: 'x'.repeat(TEXT_LIMIT), hidden: 1234 });
    expect(truncateText('short')).toEqual({ shown: 'short', hidden: 0 });
  });
  it('lists object and array entries', () => {
    expect(valueEntries({ a: 1 })).toEqual([['a', 1]]);
    expect(valueEntries(['x', 'y'])).toEqual([
      ['0', 'x'],
      ['1', 'y'],
    ]);
  });
});
