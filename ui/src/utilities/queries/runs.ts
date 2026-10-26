//! Query hooks for runs (reactions): the list keys a launch makes stale, and for one run its reaction, status updates,
//! stage logs, sub reactions, and pipeline order.
import { useEffect, useMemo, useRef } from 'react';
import { Query, QueryClient, useQuery, useQueryClient } from '@tanstack/react-query';

// project imports
import { getPipeline } from '@thorpi/pipelines';
import { getReaction, getReactionLogs } from '@thorpi/reactions';
import { getRunStageLogs, listSubRuns } from '@thorpi/runs';
import { isRunFinished, runKind } from '@utilities/runs';
import { Reaction, StatusUpdate } from '@models/reactions';
import { RunKind } from '@models/runs';

/** Query keys of the run and reaction lists, which a launch makes stale. */
export const runListKeys = {
  /** Every run list query */
  runs: ['runs'] as const,
  /** Every reaction list query */
  reactions: ['reactions'] as const,
};

/**
 * Mark every run and reaction list stale, so a list shown after a launch reads the new runs.
 *
 * @param client - The query client.
 * @returns A promise that settles once the lists in use have been read again.
 */
export async function invalidateRunLists(client: QueryClient): Promise<void> {
  await Promise.all([
    client.invalidateQueries({ queryKey: runListKeys.runs }),
    client.invalidateQueries({ queryKey: runListKeys.reactions }),
  ]);
}

/** The query key of a reaction. */
export const reactionKey = (group: string, id: string) => ['reaction', group, id] as const;
/** The query key of a reaction's status updates. */
export const reactionUpdatesKey = (group: string, id: string) => ['reaction-updates', group, id] as const;
/** The query key of one stage's log lines. */
export const reactionLogsKey = (group: string, id: string, stage: string) => ['reaction-logs', group, id, stage] as const;
/** The query key of a reaction's sub reactions. */
export const reactionSubRunsKey = (group: string, id: string) => ['reaction-sub-runs', group, id] as const;
/** The query key of a pipeline's stage order. */
export const pipelineOrderKey = (group: string, name: string) => ['pipeline-order', group, name] as const;

/** A failed read, with the API's message and HTTP status. */
export class RunReadError extends Error {
  /** The HTTP status the API answered with, when it answered */
  status?: number;

  /**
   * Create a read error.
   *
   * @param message - The formatted error message.
   * @param status - The HTTP status, when the API answered.
   */
  constructor(message: string, status?: number) {
    super(message);
    this.name = 'RunReadError';
    this.status = status;
  }
}

/** The HTTP statuses that mean the run will not become readable by asking again: not signed in for it, forbidden, and not found. */
const PERMANENT_STATUSES = [401, 403, 404];

/** The most reads of a run that may fail in a row before polling stops, so a deleted run does not poll forever. */
export const MAX_POLL_FAILURES = 5;

/**
 * Tell whether a read failure is one no retry fixes.
 *
 * @param error - The query's error.
 * @returns Whether the API refused or could not find the run.
 */
export function isGone(error: unknown): boolean {
  return error instanceof RunReadError && error.status !== undefined && PERMANENT_STATUSES.includes(error.status);
}

/**
 * Read a reaction, throwing when the read fails so the query keeps the last reaction read and reports the error.
 *
 * @param group - The reaction's group.
 * @param id - The reaction's id.
 * @returns The reaction.
 * @throws {RunReadError} When the read fails.
 */
export async function fetchReaction(group: string, id: string): Promise<Reaction> {
  let message = 'The run could not be read';
  let status: number | undefined;
  const found = await getReaction(
    group,
    id,
    (error) => (message = error),
    (code) => (status = code),
  );
  if (!found) throw new RunReadError(message, status);
  return found;
}

/** How many status updates one page reads. */
export const UPDATES_PAGE_SIZE = 1000;

/** The most pages of status updates read, so a runaway log can't loop forever. */
export const MAX_UPDATE_PAGES = 20;

/**
 * Read all of a reaction's status updates, page by page.
 *
 * @param group - The reaction's group.
 * @param id - The reaction's id.
 * @returns The status updates in the order the API keeps them.
 * @throws {RunReadError} When a read fails.
 */
export async function fetchAllStatusUpdates(group: string, id: string): Promise<StatusUpdate[]> {
  const updates: StatusUpdate[] = [];
  for (let page = 0; page < MAX_UPDATE_PAGES; page++) {
    let message = 'The status updates could not be read';
    // the cursor is the offset of the next update to read
    const cursor = page === 0 ? null : String(updates.length);
    const found = await getReactionLogs(group, id, (error) => (message = error), cursor, UPDATES_PAGE_SIZE);
    if (found === null) throw new RunReadError(message);
    // anything but a list (an empty object from a stub, for one) holds no updates
    const list = Array.isArray(found) ? found : [];
    updates.push(...list);
    if (list.length < UPDATES_PAGE_SIZE) break;
  }
  return updates;
}

/**
 * Read one stage's log lines.
 *
 * @param group - The reaction's group.
 * @param id - The reaction's id.
 * @param stage - The stage's name.
 * @returns The log lines.
 * @throws {RunReadError} When the read fails.
 */
export async function fetchStageLogs(group: string, id: string, stage: string): Promise<string[]> {
  let message = 'The stage logs could not be read';
  const found = await getRunStageLogs(group, id, stage, (error) => (message = error));
  if (found === null) throw new RunReadError(message);
  return found;
}

/**
 * Read a reaction's sub reactions; a listing that read some pages before failing keeps what it read.
 *
 * @param group - The reaction's group.
 * @param id - The reaction's id.
 * @returns The sub reactions.
 * @throws {RunReadError} When the first page fails.
 */
export async function fetchSubRuns(group: string, id: string): Promise<Reaction[]> {
  let message = '';
  const found = await listSubRuns(group, id, (error) => (message = error));
  if (found.length === 0 && message !== '') throw new RunReadError(message);
  return found;
}

/**
 * Read a pipeline's stage order.
 *
 * @param group - The pipeline's group.
 * @param name - The pipeline's name.
 * @returns The stage names of each step.
 * @throws {RunReadError} When the read fails.
 */
export async function fetchPipelineOrder(group: string, name: string): Promise<string[][]> {
  let message = 'The pipeline could not be read';
  const pipeline = await getPipeline(group, name, (error) => (message = error));
  if (!pipeline) throw new RunReadError(message);
  // a stage is either one image or a list of images that run in parallel
  return (pipeline.order ?? []).map((step) => (typeof step === 'string' ? [step] : step));
}

/**
 * Choose how soon a run is read again.
 *
 * @param query - The run's query state: the run read last and the last error.
 * @param failures - How many reads failed in a row.
 * @param pollMs - The polling period.
 * @returns The delay in milliseconds, or `false` once the run finished, the API refused or lost it, or too many reads failed.
 */
export function runRefetchInterval(query: { data?: Reaction; error: unknown }, failures: number, pollMs: number): number | false {
  if (query.data && isRunFinished(query.data.status)) return false;
  if (isGone(query.error) || failures >= MAX_POLL_FAILURES) return false;
  return pollMs;
}

/** What {@link useReactionQuery} gives a page. */
export interface ReactionQueryResult {
  /** The reaction as read last, or `null` before the first read */
  run: Reaction | null;
  /** The message of the last failed read, or an empty string */
  error: string;
  /** Whether the API refused or could not find the reaction */
  gone: boolean;
}

/**
 * Read a reaction and keep reading it until it finishes.
 *
 * @param group - The reaction's group.
 * @param id - The reaction's id.
 * @param pollMs - The polling period while it works.
 * @param enabled - Whether to read it at all (a deleted reaction is not read again).
 * @returns The reaction, the last error, and whether it is gone.
 */
export function useReactionQuery(group: string, id: string, pollMs: number, enabled = true): ReactionQueryResult {
  // consecutive failures, which the query state does not count across reads
  const failures = useRef(0);
  const query = useQuery({
    queryKey: reactionKey(group, id),
    queryFn: async () => {
      try {
        const found = await fetchReaction(group, id);
        failures.current = 0;
        return found;
      } catch (error) {
        failures.current += 1;
        throw error;
      }
    },
    enabled,
    retry: false,
    refetchOnWindowFocus: false,
    refetchInterval: (state: Query<Reaction, Error>) => runRefetchInterval(state.state, failures.current, pollMs),
  });
  return {
    run: query.data ?? null,
    error: query.error ? query.error.message : '',
    gone: isGone(query.error),
  };
}

/**
 * Invalidate a query whenever a value changes after the first render.
 *
 * @param client - The query client.
 * @param queryKey - The query to invalidate.
 * @param version - The value whose changes should reload the query.
 */
function useInvalidateOnChange(client: QueryClient, queryKey: readonly unknown[], version: string) {
  const first = useRef(version);
  useEffect(() => {
    // the first render reads the query anyway
    if (first.current === version) return;
    first.current = version;
    void client.invalidateQueries({ queryKey });
  }, [client, version, queryKey]);
}

/** What a list query gives a section. */
export interface ListQueryResult<T> {
  /** The items read last, or `null` before the first read finishes */
  data: T[] | null;
  /** The message of the last failed read, or an empty string */
  error: string;
}

/**
 * Read a reaction's status updates, polling while it works and reloading when its state changes.
 *
 * @param run - The reaction.
 * @param pollMs - The polling period while it works; job claims change no reaction field, so they are polled for.
 * @returns The updates and the last error.
 */
export function useReactionUpdates(
  run: Pick<Reaction, 'group' | 'id' | 'status' | 'current_stage' | 'current_stage_progress'>,
  pollMs: number,
): ListQueryResult<StatusUpdate> {
  const client = useQueryClient();
  const key = reactionUpdatesKey(run.group, run.id);
  const running = !isRunFinished(run.status);
  useInvalidateOnChange(client, key, `${run.status}-${run.current_stage}-${run.current_stage_progress}`);
  const query = useQuery({
    queryKey: key,
    queryFn: () => fetchAllStatusUpdates(run.group, run.id),
    retry: false,
    refetchOnWindowFocus: false,
    refetchInterval: running ? pollMs : false,
  });
  return { data: query.data ?? null, error: query.error ? query.error.message : '' };
}

/** How often an open stage's logs reload while the run works, in milliseconds. */
export const LOG_REFRESH_MS = 2000;

/**
 * Read one stage's log lines, reloading them while the run works.
 *
 * @param group - The reaction's group.
 * @param id - The reaction's id.
 * @param stage - The stage's name.
 * @param running - Whether the run still works.
 * @returns The lines and the last error.
 */
export function useStageLogs(group: string, id: string, stage: string, running: boolean): ListQueryResult<string> {
  const query = useQuery({
    queryKey: reactionLogsKey(group, id, stage),
    queryFn: () => fetchStageLogs(group, id, stage),
    retry: false,
    refetchOnWindowFocus: false,
    refetchInterval: running ? LOG_REFRESH_MS : false,
  });
  return { data: query.data ?? null, error: query.error ? query.error.message : '' };
}

/**
 * Read a reaction's sub reactions, reloading when its state or sub reaction counts change.
 *
 * @param run - The reaction.
 * @returns The sub reactions and the last error.
 */
export function useSubRuns(
  run: Pick<Reaction, 'group' | 'id' | 'status' | 'sub_reactions' | 'completed_sub_reactions'>,
): ListQueryResult<Reaction> {
  const client = useQueryClient();
  const key = reactionSubRunsKey(run.group, run.id);
  useInvalidateOnChange(client, key, `${run.status}-${run.sub_reactions}-${run.completed_sub_reactions}`);
  const query = useQuery({ queryKey: key, queryFn: () => fetchSubRuns(run.group, run.id), retry: false, refetchOnWindowFocus: false });
  return { data: query.data ?? null, error: query.error ? query.error.message : '' };
}

/**
 * Get a run's stage order: the plan it recorded, or for a pipeline reaction without a plan, its pipeline's order.
 *
 * @param run - The run.
 * @returns The stage names of each step; empty until a pipeline's order is read.
 */
export function useStageOrder(run: Pick<Reaction, 'group' | 'pipeline' | 'plan' | 'source'>): string[][] {
  // every poll reads a new run object, so the order is rebuilt only when its names change, keeping the diagram's layout still
  const planKey = JSON.stringify((run.plan?.stages ?? []).map((step) => step.map((stage) => stage.name)));
  const planned = useMemo(() => JSON.parse(planKey) as string[][], [planKey]);
  // only a pipeline reaction from before plans were recorded needs its pipeline read
  const needsPipeline = planned.length === 0 && runKind(run) === RunKind.Pipeline && !!run.pipeline;
  const query = useQuery({
    queryKey: pipelineOrderKey(run.group, run.pipeline),
    queryFn: () => fetchPipelineOrder(run.group, run.pipeline),
    enabled: needsPipeline,
    retry: false,
    refetchOnWindowFocus: false,
    staleTime: Infinity,
  });
  return planned.length > 0 ? planned : (query.data ?? []);
}
