import client, { parseRequestError } from './client';

// project imports
import { Entities } from '@models/entities/entities';
import { Hunt, HuntCreateRequest, HuntRuleLine, HuntSystemStatus } from '@models/entities/hunts';

/**
 * A frozen rule line as it rides the wire: serde skips an empty `status` list and
 * defaults `quarantine_exempt`, so both are optional until normalization fills them.
 */
type HuntRuleLineWire = Omit<HuntRuleLine, 'status' | 'quarantine_exempt'> & {
  status?: string[];
  quarantine_exempt?: boolean;
};

/**
 * Create a hunt via the dedicated JSON route (`POST /entities/hunt/`).
 *
 * Hunts never use the generic multipart entity create: the route resolves and freezes the
 * rule selector, splits the corpus into lanes, and emits the first chunk events. The API
 * replies with the full created hunt entity.
 *
 * @param huntRequest - The hunt create request (name, groups, engine, selector, corpus?).
 * @param errorHandler - Called with a formatted message if the request fails (quota, cap,
 *                       empty selector, and corpus 400s surface the API's error text).
 * @returns The created {@link Hunt}, or `null` if the request failed.
 */
export async function createHunt(huntRequest: HuntCreateRequest, errorHandler: (error: string) => void): Promise<Hunt | null> {
  return client
    .post<Hunt>('/entities/hunt/', huntRequest)
    .then((res) => {
      if (res?.status == 200 && res.data && res.data.kind == Entities.Hunt) {
        return res.data;
      }
      return null;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Create Hunt');
      return null;
    });
}

/**
 * List one page of a hunt's frozen rules (`GET /entities/hunt/{id}/rules` — the
 * id-suffixed route takes no trailing slash, unlike the entity list routes).
 *
 * The frozen snapshot is immutable, so cursors never expire and a drained list never
 * needs refetching; each line still hydrates name/kind/created from the CURRENT rule
 * entity (absent for rules deleted after the freeze or invisible to the caller). The
 * wire's serde-skipped `status` and defaulted `quarantine_exempt` are normalized here
 * so consumers never branch on absence.
 *
 * @param id - The hunt entity's id.
 * @param cursor - Pagination cursor from a previous call, or `null` for the first page.
 * @param limit - The maximum number of lines to return in this page.
 * @param errorHandler - Called with a formatted message if the request fails (non-hunt
 *                       400s and missing-hunt 404s surface the API's error text).
 * @returns The page of normalized {@link HuntRuleLine}s and the next-page cursor
 *          (`null` when exhausted). On failure, returns an empty page and a `null` cursor.
 */
export async function listHuntRules(
  id: string,
  cursor: string | null,
  limit: number,
  errorHandler: (error: string) => void,
): Promise<{ lines: HuntRuleLine[]; cursor: string | null }> {
  const params: { limit: number; cursor?: string } = { limit };
  if (cursor) {
    params.cursor = cursor;
  }
  return client
    .get<{ cursor?: string; data: HuntRuleLineWire[] }>(`/entities/hunt/${id}/rules`, { params })
    .then((res) => {
      if (res?.status == 200 && res.data) {
        // fill the serde-skipped fields so every line downstream carries a solid shape
        const lines = res.data.data.map((line) => ({
          ...line,
          status: line.status ?? [],
          quarantine_exempt: line.quarantine_exempt ?? false,
        }));
        return { lines, cursor: res.data.cursor ?? null };
      }
      return { lines: [], cursor: null };
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'List Hunt Rules');
      return { lines: [], cursor: null };
    });
}

/**
 * Cancel a hunt (`POST /entities/hunt/{id}/cancel`).
 *
 * Submitter-or-admin only; running chunks observe the cancel lazily within one chunk.
 * Re-cancelling an already cancelled hunt is an idempotent success.
 *
 * @param id - The hunt entity's id.
 * @param errorHandler - Called with a formatted message if the request fails (terminal
 *                       hunts 400 and non-submitters 401 with the API's error text).
 * @returns True when the API acknowledged the cancel with a 204.
 */
export async function cancelHunt(id: string, errorHandler: (error: string) => void): Promise<boolean> {
  return client
    .post(`/entities/hunt/${id}/cancel`)
    .then((res) => {
      return res?.status == 204;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Cancel Hunt');
      return false;
    });
}

/**
 * Fetch the admin hunt census (`GET /entities/hunt/status/`); admin only.
 *
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param recent - The max number of recently terminal hunts to return (server caps at 100).
 * @param windowDays - How many days of created hunts the recent window covers (server caps at 30).
 * @returns The {@link HuntSystemStatus} census, or `null` if the request failed.
 */
export async function getHuntSystemStatus(
  errorHandler: (error: string) => void,
  recent?: number,
  windowDays?: number,
): Promise<HuntSystemStatus | null> {
  // the census route requires the trailing slash (the bare path is a 404)
  const url = '/entities/hunt/status/';
  // only send the bounds the caller actually chose
  const params: { recent?: number; window_days?: number } = {};
  if (recent !== undefined) {
    params.recent = recent;
  }
  if (windowDays !== undefined) {
    params.window_days = windowDays;
  }
  return client
    .get<HuntSystemStatus>(url, { params })
    .then((res) => {
      // require the census shape so a half-broken API can never crash the page
      if (res?.status == 200 && res.data && res.data.quotas) {
        return res.data;
      }
      return null;
    })
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Hunt System Status');
      return null;
    });
}
