import { describe, it, expect, vi, beforeEach, Mock } from 'vitest';

// project imports (the setup file auto-mocks @thorpi/client)
import client, { parseRequestError } from './client';
import { getEventStats, getEventStatsHistory } from './events';
import { EventStats, ScanStatsSnapshot } from '@models/events';

// the auto-mocked client's get is a plain vi.fn() with no `this`, so unbinding is safe
// eslint-disable-next-line @typescript-eslint/unbound-method
const mockedGet = client.get as Mock;
const mockedParse = parseRequestError as Mock;

/// A minimal live aggregate for mock responses.
const stats: EventStats = {
  taken_at: '2026-10-23T00:00:00Z',
  workers: 2,
  kinds: {},
  per_worker: {},
};

/// A minimal history row for mock responses.
const snapshot: ScanStatsSnapshot = {
  snapshot_at: '2026-10-23T00:00:00Z',
  workers: 2,
  kinds: {},
};

describe('getEventStats', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('gets the live aggregate from the trailing-slash stats route', async () => {
    mockedGet.mockResolvedValue({ status: 200, data: stats });
    const live = await getEventStats(() => {});
    expect(mockedGet).toHaveBeenCalledWith('/events/stats/');
    expect(live?.workers).toBe(2);
  });

  it('funnels failures through parseRequestError and returns null', async () => {
    mockedGet.mockRejectedValue(new Error('denied'));
    const errorHandler = vi.fn();
    expect(await getEventStats(errorHandler)).toBeNull();
    expect(mockedParse).toHaveBeenCalledWith(expect.any(Error), errorHandler, 'Get Event Stats');
  });
});

describe('getEventStatsHistory', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('gets the half-open range and omits an unset stride', async () => {
    mockedGet.mockResolvedValue({ status: 200, data: [snapshot] });
    const rows = await getEventStatsHistory('2026-10-22T00:00:00Z', '2026-10-23T00:00:00Z', undefined, () => {});
    // the stride param must be absent when the caller wants the server default
    expect(mockedGet).toHaveBeenCalledWith('/events/stats/history/', {
      params: { start: '2026-10-22T00:00:00Z', end: '2026-10-23T00:00:00Z' },
    });
    expect(rows?.length).toBe(1);
  });

  it('passes a requested stride through as step_secs', async () => {
    mockedGet.mockResolvedValue({ status: 200, data: [] });
    await getEventStatsHistory('a', 'b', 60, () => {});
    expect(mockedGet).toHaveBeenCalledWith('/events/stats/history/', {
      params: { start: 'a', end: 'b', step_secs: 60 },
    });
  });

  it('funnels failures through parseRequestError and returns null', async () => {
    mockedGet.mockRejectedValue(new Error('denied'));
    const errorHandler = vi.fn();
    expect(await getEventStatsHistory('a', 'b', undefined, errorHandler)).toBeNull();
    expect(mockedParse).toHaveBeenCalledWith(expect.any(Error), errorHandler, 'Get Event Stats History');
  });
});
