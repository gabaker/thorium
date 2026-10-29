import { describe, it, expect } from 'vitest';

// project imports
import { formatTimeTick, nearestTsIndex } from './VisxLineChart';

describe('nearestTsIndex', () => {
  it('returns -1 for an empty timestamp list', () => {
    expect(nearestTsIndex([], 100)).toBe(-1);
  });

  it('picks the closest timestamp and keeps the earlier point on ties', () => {
    const timestamps = [0, 100, 200];
    expect(nearestTsIndex(timestamps, 90)).toBe(1);
    expect(nearestTsIndex(timestamps, 201)).toBe(2);
    // an exact midpoint tie snaps to the earlier point
    expect(nearestTsIndex(timestamps, 50)).toBe(0);
  });
});

describe('formatTimeTick', () => {
  it('uses clock times for short spans and dates for multi-day spans', () => {
    const ts = Date.parse('2026-10-23T12:34:00Z');
    const shortSpan = formatTimeTick(ts, 60 * 60 * 1000);
    const longSpan = formatTimeTick(ts, 7 * 24 * 60 * 60 * 1000);
    // short spans carry a clock separator while long spans drop it for a date
    expect(shortSpan).toContain(':');
    expect(longSpan).not.toContain(':');
  });
});
