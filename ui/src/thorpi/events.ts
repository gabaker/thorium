import client, { parseRequestError } from './client';

// project imports
import { EventStats, ScanStatsSnapshot } from '@models/events';

/**
 * Fetch the live scan stats aggregate (`GET /events/stats/`); admin only.
 *
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The live {@link EventStats}, or `null` if the request failed.
 */
export async function getEventStats(errorHandler: (error: string) => void): Promise<EventStats | null> {
  // both stats routes require the trailing slash (the bare path is a 404)
  const url = '/events/stats/';
  return client
    .get<EventStats>(url)
    .then((res) => {
      // require the aggregate shape so a half-broken API can never crash the page
      if (res?.status == 200 && res.data && typeof res.data.workers === 'number' && res.data.kinds) {
        return res.data;
      }
      return null;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Event Stats');
      return null;
    });
}

/**
 * Fetch a ranged, downsampled slice of the scan stats history
 * (`GET /events/stats/history/`); admin only.
 *
 * The range is half open (`start <= snapshot_at < end`) and rows come back
 * oldest first, downsampled to the newest row per stride.
 *
 * @param start - The oldest instant to return rows from (RFC3339, inclusive).
 * @param end - The newest instant to return rows up to (RFC3339, exclusive).
 * @param stepSecs - The stride to downsample to in seconds; omit for the
 *                   server default of roughly 1,000 points across the range.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The history rows oldest first, or `null` if the request failed.
 */
export async function getEventStatsHistory(
  start: string,
  end: string,
  stepSecs: number | undefined,
  errorHandler: (error: string) => void,
): Promise<ScanStatsSnapshot[] | null> {
  const url = '/events/stats/history/';
  // build the half open range params; axios url-encodes the RFC3339 strings
  const params: { start: string; end: string; step_secs?: number } = { start, end };
  if (stepSecs !== undefined) {
    params.step_secs = stepSecs;
  }
  return client
    .get<ScanStatsSnapshot[]>(url, { params })
    .then((res) => {
      // history must be an array; anything else renders as the collecting state
      if (res?.status == 200 && Array.isArray(res.data)) {
        return res.data;
      }
      return null;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Event Stats History');
      return null;
    });
}
