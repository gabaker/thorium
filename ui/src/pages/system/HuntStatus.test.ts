import { describe, it, expect } from 'vitest';

// project imports
import {
  PRIMARY_RATE_COUNTERS,
  deriveQueueSeries,
  deriveRateSeries,
  gapThresholdMs,
  historyRefreshMs,
  liveRate,
  medianSpacingMs,
  snapshotsAreStale,
} from './HuntStatus';
import { EventStats, EventType, ScanStatsSnapshot } from '@models/events';

/**
 * Build a snapshot carrying one yara-scan counter value at an offset.
 *
 * @param offsetSecs - Seconds after the fixed base this snapshot covers.
 * @param scanned - The cumulative yara `scanned` counter to carry.
 * @param queued - The yara kind's queued depth to carry.
 * @returns The snapshot row.
 */
function snap(offsetSecs: number, scanned: number, queued = 0): ScanStatsSnapshot {
  return {
    snapshot_at: new Date(Date.parse('2026-10-23T00:00:00Z') + offsetSecs * 1000).toISOString(),
    workers: 1,
    kinds: {
      [EventType.YaraScannableSample]: { queued, in_flight: 0, counters: { scanned } },
    },
  };
}

/**
 * Build a live aggregate carrying one yara-scan counter value.
 *
 * @param takenAt - The RFC3339 assembly time.
 * @param scanned - The cumulative yara `scanned` counter to carry.
 * @returns The live aggregate.
 */
function live(takenAt: string, scanned: number): EventStats {
  return {
    taken_at: takenAt,
    workers: 1,
    kinds: { [EventType.YaraScannableSample]: { queued: 0, in_flight: 0, counters: { scanned } } },
    per_worker: {},
  };
}

describe('PRIMARY_RATE_COUNTERS', () => {
  it('pins the primary counter name per kind', () => {
    // these names are the wire contract with the worker stats structs
    expect(PRIMARY_RATE_COUNTERS[EventType.ReactionTrigger]).toBe('seen');
    expect(PRIMARY_RATE_COUNTERS[EventType.SigmaScannableResults]).toBe('total.scanned');
    expect(PRIMARY_RATE_COUNTERS[EventType.YaraScannableSample]).toBe('scanned');
    expect(PRIMARY_RATE_COUNTERS[EventType.YaraHuntChunk]).toBe('scanned');
  });
});

describe('medianSpacingMs / gapThresholdMs', () => {
  it('derives the cadence from the data and refuses with too few points', () => {
    expect(medianSpacingMs([0])).toBeNull();
    expect(medianSpacingMs([0, 30_000, 60_000])).toBe(30_000);
    // the gap threshold is 2.5x the observed cadence, never a configured value
    expect(gapThresholdMs([0, 30_000, 60_000])).toBe(75_000);
    expect(gapThresholdMs([0])).toBe(Number.POSITIVE_INFINITY);
  });
});

describe('deriveRateSeries', () => {
  it('divides counter deltas by the real spacing between readings', () => {
    const points = deriveRateSeries([snap(0, 0), snap(30, 60), snap(60, 90)], EventType.YaraScannableSample);
    expect(points.map((point) => point.value)).toEqual([2, 1]);
  });

  it('clamps negative deltas so a worker restart is a dip, never a spike', () => {
    const points = deriveRateSeries([snap(0, 1_000), snap(30, 10), snap(60, 40)], EventType.YaraScannableSample);
    // the restart pair clamps to zero and the next pair resumes real rates
    expect(points.map((point) => point.value)).toEqual([0, 1]);
  });

  it('breaks the line across gaps longer than the derived spacing', () => {
    // steady 30s cadence with one dead window far beyond 2.5x that spacing
    const snapshots = [snap(0, 0), snap(30, 30), snap(60, 60), snap(300, 90), snap(330, 120)];
    const points = deriveRateSeries(snapshots, EventType.YaraScannableSample);
    // the pair spanning the dead window is a null gap, not an interpolated rate
    expect(points.map((point) => point.value)).toEqual([1, 1, null, 1]);
  });
});

describe('deriveQueueSeries', () => {
  it('carries one gauge point per snapshot and breaks across gaps', () => {
    const snapshots = [snap(0, 0, 5), snap(30, 0, 7), snap(300, 0, 2), snap(330, 0, 3)];
    const points = deriveQueueSeries(snapshots, EventType.YaraScannableSample);
    // a null breaker splits the two sampled stretches
    expect(points.map((point) => point.value)).toEqual([5, 7, null, 2, 3]);
  });
});

describe('liveRate', () => {
  it('needs two readings and sums clamped deltas across kinds', () => {
    expect(liveRate(null, live('2026-10-23T00:00:10Z', 50))).toBeNull();
    const rate = liveRate(live('2026-10-23T00:00:00Z', 50), live('2026-10-23T00:00:10Z', 70));
    expect(rate).toBe(2);
    // a restart between readings clamps to zero instead of going negative
    const clamped = liveRate(live('2026-10-23T00:00:00Z', 500), live('2026-10-23T00:00:10Z', 10));
    expect(clamped).toBe(0);
  });
});

describe('snapshotsAreStale', () => {
  it('treats an empty history as collecting, not stale', () => {
    expect(snapshotsAreStale([], '2026-10-23T01:00:00Z', 0)).toBe(false);
  });

  it('flags a live view far past the newest snapshot', () => {
    const snapshots = [snap(0, 0), snap(30, 10), snap(60, 20)];
    // just past the newest row is fresh; far past the derived cadence is stale
    expect(snapshotsAreStale(snapshots, new Date(Date.parse('2026-10-23T00:00:00Z') + 90_000).toISOString(), 0)).toBe(false);
    expect(snapshotsAreStale(snapshots, new Date(Date.parse('2026-10-23T00:00:00Z') + 600_000).toISOString(), 0)).toBe(true);
  });

  it('tolerates the charts own refresh interval before crying stale', () => {
    const snapshots = [snap(0, 0), snap(30, 10), snap(60, 20)];
    // cadence 30s derives a 75s gap threshold; with a 60s refresh interval the
    // newest row may age to 135s before the note shows
    const base = Date.parse('2026-10-23T00:00:00Z');
    expect(snapshotsAreStale(snapshots, new Date(base + 60_000 + 120_000).toISOString(), 60_000)).toBe(false);
    expect(snapshotsAreStale(snapshots, new Date(base + 60_000 + 140_000).toISOString(), 60_000)).toBe(true);
  });
});

describe('historyRefreshMs', () => {
  it('refreshes at the charts information rate with a 60s floor', () => {
    // short ranges floor at a minute; longer ranges track their ~1,000 point
    // stride so refreshes never outpace the data
    expect(historyRefreshMs(3_600)).toBe(60_000);
    expect(historyRefreshMs(21_600)).toBe(60_000);
    expect(historyRefreshMs(86_400)).toBe(86_400);
    expect(historyRefreshMs(604_800)).toBe(604_800);
  });
});
