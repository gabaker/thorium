import { describe, expect, it } from 'vitest';

// project imports
import { describeTime, formatRelativeTimestamp, parseTimestamp } from './time';

/** A fixed now: Oct 29, 2026, 11:05 UTC. */
const NOW = Date.parse('2026-10-29T11:05:00Z');
/** Format pinned to US English in UTC. */
const OPTS = { now: NOW, locale: 'en-US', timeZone: 'UTC' };

describe('parseTimestamp', () => {
  it('reads RFC 3339 text with nanoseconds', () => {
    expect(parseTimestamp('2026-10-29T10:26:06.176140017Z')).toBe(Date.parse('2026-10-29T10:26:06.176Z'));
  });
  it('reads the space separated UTC form status updates use', () => {
    expect(parseTimestamp('2026-11-05 20:26:06.175074104 UTC')).toBe(Date.parse('2026-11-05T20:26:06.175Z'));
  });
  it('gives NaN for text that is not a date', () => {
    expect(parseTimestamp('soon')).toBeNaN();
  });
});

describe('describeTime', () => {
  it('describes a future time with its local date and time', () => {
    const described = describeTime('2026-10-29T14:05:00Z', OPTS);
    expect(described.relative).toBe('in 3 hours');
    expect(described.absolute).toBe('Oct 29, 14:05');
    expect(described.past).toBe(false);
    expect(described.iso).toBe('2026-10-29T14:05:00.000Z');
  });
  it('describes a past time', () => {
    const described = describeTime('2026-10-27T14:05:00Z', OPTS);
    expect(described.relative).toBe('2 days ago');
    expect(described.past).toBe(true);
  });
  it('shows the year only for another year', () => {
    expect(describeTime('2027-01-02T08:00:00Z', OPTS).absolute).toBe('Jan 2, 2027, 08:00');
  });
  it('marks an overdue time as past', () => {
    // an SLA a minute ago is overdue
    expect(describeTime('2026-10-29T11:04:00Z', OPTS)).toMatchObject({ relative: '1 minute ago', past: true });
  });
  it('keeps an invalid date as it is', () => {
    expect(describeTime('not a date', OPTS)).toEqual({
      valid: false,
      relative: 'not a date',
      absolute: 'not a date',
      iso: 'not a date',
      past: false,
    });
  });
});

describe('formatRelativeTimestamp', () => {
  it('puts the absolute time in parentheses after the relative one', () => {
    expect(formatRelativeTimestamp('2026-10-30T14:05:00Z', OPTS)).toBe('tomorrow (Oct 30, 14:05)');
  });
  it('returns text that is not a date unchanged', () => {
    expect(formatRelativeTimestamp('later', OPTS)).toBe('later');
  });
});
