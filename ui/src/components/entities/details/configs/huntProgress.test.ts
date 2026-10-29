import { describe, it, expect } from 'vitest';

// project imports
import { laneFraction, overallProgress } from './huntProgress';
import { HuntLane } from '@models/entities/hunts';

// build a lane spanning [end, start] with current at a given position
const lane = (start: string, current: string, end: string, exhausted = false): HuntLane => ({
  start,
  current,
  end,
  seq: 0,
  cursor: null,
  exhausted,
});

describe('laneFraction', () => {
  it('is 0 when the lane has not moved (current == start)', () => {
    expect(laneFraction(lane('2026-01-02T00:00:00Z', '2026-01-02T00:00:00Z', '2026-01-01T00:00:00Z'))).toBe(0);
  });

  it('is the time fraction walked between start and end', () => {
    // current is 6 hours into a 24 hour lane
    expect(laneFraction(lane('2026-01-02T00:00:00Z', '2026-01-01T18:00:00Z', '2026-01-01T00:00:00Z'))).toBeCloseTo(0.25);
  });

  it('clamps a current newer than start to 0', () => {
    expect(laneFraction(lane('2026-01-02T00:00:00Z', '2026-01-03T00:00:00Z', '2026-01-01T00:00:00Z'))).toBe(0);
  });

  it('clamps a current older than end to 1', () => {
    expect(laneFraction(lane('2026-01-02T00:00:00Z', '2025-12-31T00:00:00Z', '2026-01-01T00:00:00Z'))).toBe(1);
  });

  it('is 1 for an exhausted lane regardless of current', () => {
    expect(laneFraction(lane('2026-01-02T00:00:00Z', '2026-01-02T00:00:00Z', '2026-01-01T00:00:00Z', true))).toBe(1);
  });

  it('treats a zero or negative width lane as done', () => {
    expect(laneFraction(lane('2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'))).toBe(1);
    expect(laneFraction(lane('2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z'))).toBe(1);
  });

  it('is 0 for unparseable timestamps (the blank placeholder)', () => {
    expect(laneFraction(lane('', '', ''))).toBe(0);
  });
});

describe('overallProgress', () => {
  it('is 0 with no lanes', () => {
    expect(overallProgress([])).toBe(0);
  });

  it('weights lanes by their time width', () => {
    // a 24h lane at 50% and a 12h lane at 100%: (24*0.5 + 12*1) / 36 = 2/3
    const lanes = [
      lane('2026-01-02T00:00:00Z', '2026-01-01T12:00:00Z', '2026-01-01T00:00:00Z'),
      lane('2026-01-01T00:00:00Z', '2025-12-31T12:00:00Z', '2025-12-31T12:00:00Z'),
    ];
    expect(overallProgress(lanes)).toBeCloseTo(2 / 3);
  });

  it('skips degenerate lanes in the weighting', () => {
    const lanes = [
      // zero width: contributes no weight
      lane('2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'),
      // 24h lane fully walked
      lane('2026-01-02T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'),
    ];
    expect(overallProgress(lanes)).toBe(1);
  });

  it('is 1 when every lane is degenerate but exhausted', () => {
    expect(overallProgress([lane('2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', true)])).toBe(1);
  });

  it('is 0 when every lane is degenerate and not exhausted', () => {
    expect(overallProgress([lane('2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')])).toBe(0);
  });

  it('is 1 when every lane is exhausted', () => {
    const lanes = [
      lane('2026-01-02T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', true),
      lane('2026-01-01T00:00:00Z', '2025-12-31T00:00:00Z', '2025-12-31T00:00:00Z', true),
    ];
    expect(overallProgress(lanes)).toBe(1);
  });
});
