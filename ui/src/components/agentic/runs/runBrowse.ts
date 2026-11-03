//! The runs list's reads: the first page of every listing of the user's groups, kept in the query cache and refreshed
//! while a run is unfinished, and the further pages Load more reads into the same cache entry.
import { Query, useQuery, useQueryClient } from '@tanstack/react-query';
import { useCallback, useMemo, useState } from 'react';

// project imports
import { mergeRuns, refreshedRows, staleUnfinished } from './display';
import { listRunsByStatus, listRunsByTag, RunPage } from '@thorpi/runs';
import { mapWithConcurrency } from '@utilities/concurrency';
import { rereadRuns, runListKeys } from '@utilities/queries/runs';
import { isRunFinished } from '@utilities/runs';
import { Reaction, ReactionStatus } from '@models/reactions';

/** The tag every workflow run carries. */
const WORKFLOW_RUN_TAG = 'WorkflowRun';

/** How soon an unfinished list reloads after a change, in milliseconds. */
export const REFRESH_MS = 3000;

/** The longest wait between reloads of an unfinished list, in milliseconds. */
export const MAX_REFRESH_MS = 30_000;

/** How much longer each reload of an unchanged list waits than the one before. */
export const REFRESH_BACKOFF = 1.5;

/** The most listing pages read at once, so a user in many groups doesn't send every listing together. */
const LIST_CONCURRENCY = 4;

/** The listings read for each group: the workflow run tag, and each status. */
const SOURCES: (typeof WORKFLOW_RUN_TAG | ReactionStatus)[] = [WORKFLOW_RUN_TAG, ...Object.values(ReactionStatus)];

/** Where the next page of one group's listing starts. */
export interface PageCursor {
  /** The group the listing is of */
  group: string;
  /** The listing: the workflow run tag or a status */
  source: (typeof SOURCES)[number];
  /** The cursor of the next page */
  cursor: number;
}

/**
 * The query key of the runs list of a set of groups; it sits under the run list keys, so a launch makes it stale.
 *
 * @param groups - The groups listed, in any order.
 * @returns The query key, the same for the same groups in any order.
 */
export const runBrowseKey = (groups: string[]) => [...runListKeys.runs, 'browse', [...groups].sort().join(',')] as const;

/**
 * Read one page of one group's listing.
 *
 * @param group - The group to list.
 * @param source - The listing to read: the workflow run tag, or a status.
 * @param cursor - The cursor of the page, or `undefined` for the first page.
 * @param onError - Called with a formatted message if the request fails.
 * @returns The page, or `null` if the request failed.
 */
function readPage(
  group: string,
  source: PageCursor['source'],
  cursor: number | undefined,
  onError: (message: string) => void,
): Promise<RunPage | null> {
  // workflow runs are found by their tag, the other kinds by status and source
  return source === WORKFLOW_RUN_TAG ? listRunsByTag(group, source, onError, cursor) : listRunsByStatus(group, source, onError, cursor);
}

/** What one sweep read: the first page of every listing and, on a refresh, the stale shown rows read again by id. */
export interface SweepRead {
  /** The first page of each listing that answered */
  lists: Reaction[][];
  /** Where the next page of each listing with more runs starts */
  next: PageCursor[];
  /** The stale shown runs read again by id */
  reread: Reaction[];
  /** The ids of stale shown runs the API no longer finds */
  gone: string[];
  /** How many listings the sweep read */
  requests: number;
  /** How many listing reads failed; only these decide whether a refresh's pages are used */
  failures: number;
  /** How many reads of stale rows by id failed for a reason other than the run being gone */
  rereadFailures: number;
}

/**
 * Read the first page of every listing of the groups and, on a refresh, read again by id the shown unfinished rows no
 * first page holds anymore, so they show their new status or leave when gone.
 *
 * @param groups - The groups to list.
 * @param shown - The rows shown before a refresh, or `null` for a full load, which reads nothing again.
 * @param onError - Called with the message of each failed read.
 * @returns What the sweep read, with the count of listing requests and of the listing and re-read failures.
 */
export async function readSweep(groups: string[], shown: Reaction[] | null, onError: (message: string) => void): Promise<SweepRead> {
  let failures = 0;
  let rereadFailures = 0;
  // count each failed listing read before reporting it
  const failed = (message: string) => {
    failures += 1;
    onError(message);
  };
  // re-read failures are counted apart, so rows that fail to read again never make a refresh drop pages its listings read
  const rereadFailed = (message: string) => {
    rereadFailures += 1;
    onError(message);
  };
  const lists: Reaction[][] = [];
  const next: PageCursor[] = [];
  // every listing of every group, read a few at a time
  const listings = groups.flatMap((group) => SOURCES.map((source) => ({ group, source })));
  const requests = listings.length;
  const pages = await mapWithConcurrency(listings, LIST_CONCURRENCY, ({ group, source }) => readPage(group, source, undefined, failed));
  pages.forEach((page, index) => {
    if (!page) return;
    lists.push(page.runs);
    if (page.cursor !== null) next.push({ ...listings[index], cursor: page.cursor });
  });
  // the stale rows are read again only when some listing answered, so a refresh with the API down reads nothing more
  let reread: Reaction[] = [];
  let gone: string[] = [];
  if (shown !== null && failures < requests) {
    ({ found: reread, gone } = await rereadRuns(staleUnfinished(shown, lists), rereadFailed));
  }
  return { lists, next, reread, gone, requests, failures, rereadFailures };
}

/**
 * Tell whether a refresh's pages replace the shown rows.
 *
 * @param sweep - The refresh's listing request and failure counts.
 * @returns Whether a listing answered, or there was nothing to list; a refresh where every listing failed keeps the rows.
 */
export function usesRefresh(sweep: Pick<SweepRead, 'requests' | 'failures'>): boolean {
  return sweep.requests === 0 || sweep.failures < sweep.requests;
}

/**
 * Tell which runs of a list are in which status, so a reload can tell whether anything moved.
 *
 * @param runs - The runs read.
 * @returns A key that changes when a run is added, removed, or changes status.
 */
function runsSignature(runs: Reaction[]): string {
  return runs
    .map((run) => `${run.id}:${run.status}`)
    .sort()
    .join(',');
}

/**
 * Choose how long an unfinished list waits before its next reload.
 *
 * @param changed - Whether the last reload found a run added, removed, or in a new status.
 * @param previous - The wait before the last reload.
 * @returns The short wait after a change, or a longer wait (up to {@link MAX_REFRESH_MS}) after a reload that found
 *   nothing new, so a run stuck unfinished does not hold the API busy.
 */
export function nextRefreshWait(changed: boolean, previous: number): number {
  return changed ? REFRESH_MS : Math.min(previous * REFRESH_BACKOFF, MAX_REFRESH_MS);
}

/** The runs list of a set of groups as the query cache keeps it. */
export interface RunBrowse {
  /** The rows read so far, latest SLA expiry first */
  runs: Reaction[];
  /** Where the next page of each listing with more runs starts */
  more: PageCursor[];
  /** The message of the last failed read of the newest sweep or page read, or an empty string */
  error: string;
  /** Which runs the first pages held in which status at the last sweep, to tell whether a refresh found any change */
  signature: string;
  /** How long to wait before the next reload while a run is unfinished, in milliseconds */
  wait: number;
}

/**
 * Read the runs list of the groups: a full load when nothing is cached, otherwise a refresh that merges the first pages
 * and the stale rows read again into the rows cached when the sweep ends (so pages Load more read meanwhile stay).
 *
 * @param groups - The groups to list.
 * @param previous - The list cached when the read starts, or `undefined` for a full load.
 * @param latest - Get the list cached now, which a Load more may have grown while the sweep ran.
 * @returns The new list; never throws, so a failed read keeps its message beside the rows.
 */
export async function readRunBrowse(
  groups: string[],
  previous: RunBrowse | undefined,
  latest: () => RunBrowse | undefined,
): Promise<RunBrowse> {
  let error = '';
  const sweep = await readSweep(groups, previous ? previous.runs : null, (message) => (error = message));
  const { lists, next, reread, gone } = sweep;
  const seen = runsSignature([...lists.flat(), ...reread]);
  // a full load replaces the rows and cursors even when every request failed, so no rows of an earlier read stay
  if (!previous) return { runs: mergeRuns(lists), more: next, error, signature: seen, wait: REFRESH_MS };
  const base = latest() ?? previous;
  // a refresh where every request failed keeps the rows, so refreshing goes on and can restore them
  const runs = usesRefresh(sweep) ? refreshedRows(base.runs, lists, reread, gone) : base.runs;
  return { runs, more: base.more, error, signature: seen, wait: nextRefreshWait(seen !== previous.signature, previous.wait) };
}

/** What Load more read: the rows of the next pages and where the pages after them start. */
export interface MoreRead {
  /** The rows of each page that answered */
  lists: Reaction[][];
  /** The cursors still to read: a failed page keeps its cursor so the next click retries it */
  remaining: PageCursor[];
  /** The message of the last failed page read, or an empty string */
  error: string;
}

/**
 * Read the next page of every listing that has one.
 *
 * @param more - Where the next page of each listing starts.
 * @returns The rows read, the cursors left, and the last error.
 */
export async function readMoreRuns(more: PageCursor[]): Promise<MoreRead> {
  let error = '';
  const lists: Reaction[][] = [];
  const remaining: PageCursor[] = [];
  const pages = await mapWithConcurrency(more, LIST_CONCURRENCY, (entry) =>
    readPage(entry.group, entry.source, entry.cursor, (message) => (error = message)),
  );
  pages.forEach((page, index) => {
    const entry = more[index];
    if (!page) {
      remaining.push(entry);
      return;
    }
    lists.push(page.runs);
    if (page.cursor !== null) remaining.push({ ...entry, cursor: page.cursor });
  });
  return { lists, remaining, error };
}

/**
 * Get how soon a cached runs list reloads.
 *
 * @param data - The cached list, if any.
 * @returns The list's wait while any of its runs is unfinished, or `false` to stop reloading.
 */
export function runBrowseInterval(data: RunBrowse | undefined): number | false {
  return data && data.runs.some((run) => !isRunFinished(run.status)) ? data.wait : false;
}

/** The runs list a page shows, and its Load more action. */
export interface RunBrowseState {
  /** The rows read so far */
  runs: Reaction[];
  /** Whether the listings hold more runs than the pages read so far */
  hasMore: boolean;
  /** The message of the last failed read, or an empty string */
  error: string;
  /** Whether the first read of these groups is in flight */
  loading: boolean;
  /** Whether Load more is reading pages for these groups */
  loadingMore: boolean;
  /** Read the next page of every listing that has one */
  loadMore: () => Promise<void>;
}

/**
 * Load the runs of the groups through the query cache: a cached list shows at once while it is read again, and an
 * unfinished list reloads (only while the tab is visible), waiting longer after each reload that found nothing new.
 *
 * @param groups - The groups to list.
 * @param enabled - Whether to read the list; the groups are unknown until the user is signed in.
 * @returns The rows, the last error, the loading flags, and Load more.
 */
export function useRunBrowse(groups: string[], enabled: boolean): RunBrowseState {
  const client = useQueryClient();
  const keyText = runBrowseKey(groups).join('\n');
  // the key is rebuilt only when the groups change, so Load more keeps one identity between renders
  const key = useMemo(() => runBrowseKey(groups), [keyText]);
  // the groups whose pages Load more is reading, so a group change shows no Load more in progress for the new groups
  const [loadingMoreKey, setLoadingMoreKey] = useState<string | null>(null);
  const query = useQuery({
    queryKey: key,
    queryFn: ({ queryKey }) =>
      readRunBrowse(groups, client.getQueryData<RunBrowse>(queryKey), () => client.getQueryData<RunBrowse>(queryKey)),
    enabled,
    retry: false,
    // only an unfinished list reloads, on its own backoff, so focusing the tab reads nothing more
    refetchOnWindowFocus: false,
    refetchInterval: (state: Query<RunBrowse>) => runBrowseInterval(state.state.data),
  });
  const more = query.data?.more ?? [];
  const loadMore = useCallback(async () => {
    const cursors = client.getQueryData<RunBrowse>(key)?.more ?? [];
    setLoadingMoreKey(keyText);
    // a retry starts without the error of the read it retries
    client.setQueryData<RunBrowse>(key, (current) => current && { ...current, error: '' });
    const read = await readMoreRuns(cursors);
    // the pages join the list of the groups they were read for, even after a group change
    client.setQueryData<RunBrowse>(
      key,
      (current) => current && { ...current, runs: mergeRuns([current.runs, ...read.lists]), more: read.remaining, error: read.error },
    );
    setLoadingMoreKey((current) => (current === keyText ? null : current));
  }, [client, key, keyText]);
  return {
    runs: query.data?.runs ?? [],
    hasMore: more.length > 0,
    error: query.data?.error ?? '',
    loading: query.isPending,
    loadingMore: loadingMoreKey === keyText,
    loadMore,
  };
}
