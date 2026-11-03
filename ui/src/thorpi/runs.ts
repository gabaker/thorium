import client, { parseRequestError } from './client';

// project imports
import { Reaction, ReactionStatus, StageLogs, StatusUpdate } from '@models/reactions';
import { RunCreated, RunResults, ToolRunRequest, WorkflowRunRequest } from '@models/runs';

interface ReactionDetailsPage {
  cursor?: number | null;
  details: Reaction[];
}

/** One page of a reaction details listing. */
export interface RunPage {
  /** The reactions on the page */
  runs: Reaction[];
  /** The cursor of the next page, or `null` when the listing has no more */
  cursor: number | null;
}

/** The most reactions a page of a runs listing holds. */
export const RUN_PAGE_SIZE = 50;

/** The most pages of a run's sub reactions that are read. */
const MAX_SUB_RUN_PAGES = 10;

/**
 * Read one page of a reaction details listing.
 *
 * @param url - The listing's route.
 * @param cursor - The cursor of the page to read, or `undefined` for the first page.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param limit - The most reactions to read.
 * @returns The page, or `null` if the request failed.
 */
async function readRunPage(
  url: string,
  cursor: number | undefined,
  errorHandler: (error: string) => void,
  limit: number,
): Promise<RunPage | null> {
  return client
    .get<ReactionDetailsPage>(url, { params: cursor === undefined ? { limit } : { limit, cursor } })
    .then((res) => (res?.status == 200 && res.data?.details ? { runs: res.data.details, cursor: res.data.cursor ?? null } : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'List Runs');
      return null;
    });
}

/**
 * Start a run of one tool instance (`POST /tools/instances/{id}/runs`).
 *
 * @param instance - The tool instance's id.
 * @param request - The inputs, group, args, tags, and SLA of the run.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The new run's reaction id, or `null` if the run was refused.
 */
export async function runTool(
  instance: string,
  request: ToolRunRequest,
  errorHandler: (error: string) => void,
): Promise<RunCreated | null> {
  return client
    .post<RunCreated>(`/tools/instances/${encodeURIComponent(instance)}/runs`, request)
    .then((res) => (res?.status == 200 && res.data ? res.data : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Run Tool');
      return null;
    });
}

/**
 * Start a run of one workflow instance (`POST /workflows/instances/{id}/runs`).
 *
 * @param instance - The workflow instance's id.
 * @param request - The inputs, group, tags, and SLA of the run.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The new run's reaction id, or `null` if the run was refused (for example no model resolved).
 */
export async function runWorkflow(
  instance: string,
  request: WorkflowRunRequest,
  errorHandler: (error: string) => void,
): Promise<RunCreated | null> {
  return client
    .post<RunCreated>(`/workflows/instances/${encodeURIComponent(instance)}/runs`, request)
    .then((res) => (res?.status == 200 && res.data ? res.data : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Run Workflow');
      return null;
    });
}

/**
 * Retry a failed workflow run as a new run on the same inputs
 * (`POST /workflows/instances/{instance}/runs/{reaction}/retry`).
 *
 * @param instance - The workflow instance the failed run ran.
 * @param reaction - The failed run's reaction id.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The new run's reaction id, or `null` if the retry was refused.
 */
export async function retryWorkflowRun(
  instance: string,
  reaction: string,
  errorHandler: (error: string) => void,
): Promise<RunCreated | null> {
  return client
    .post<RunCreated>(`/workflows/instances/${encodeURIComponent(instance)}/runs/${encodeURIComponent(reaction)}/retry`)
    .then((res) => (res?.status == 200 && res.data ? res.data : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Retry Run');
      return null;
    });
}

/**
 * Fail a run that has not finished (`POST /reactions/handle/{group}/{id}/fail`).
 *
 * The handle route answers a successful command with 202 Accepted.
 *
 * @param group - The run's group.
 * @param reaction - The run's reaction id.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The run's status after the command, or `null` if the request failed.
 */
export async function cancelRun(group: string, reaction: string, errorHandler: (error: string) => void): Promise<ReactionStatus | null> {
  return client
    .post<{ status: ReactionStatus }>(`/reactions/handle/${encodeURIComponent(group)}/${encodeURIComponent(reaction)}/fail`)
    .then((res) => (res?.status == 202 && res.data ? res.data.status : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Cancel Run');
      return null;
    });
}

/**
 * List one page of the reactions of a group carrying a tag with their details (`GET /reactions/tag/{group}/{tag}/details/`).
 *
 * @param group - The group to list.
 * @param tag - The tag to list by (workflow runs carry `WorkflowRun`).
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param cursor - The cursor of the page to read; the first page when left out.
 * @param limit - The most reactions to read.
 * @returns The page, or `null` if the request failed.
 */
export async function listRunsByTag(
  group: string,
  tag: string,
  errorHandler: (error: string) => void,
  cursor?: number,
  limit = RUN_PAGE_SIZE,
): Promise<RunPage | null> {
  return readRunPage(`/reactions/tag/${encodeURIComponent(group)}/${encodeURIComponent(tag)}/details/`, cursor, errorHandler, limit);
}

/**
 * List one page of the reactions of a group in one status with their details (`GET /reactions/group/{group}/{status}/details/`).
 *
 * @param group - The group to list.
 * @param status - The status to list.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @param cursor - The cursor of the page to read; the first page when left out.
 * @param limit - The most reactions to read.
 * @returns The page, or `null` if the request failed.
 */
export async function listRunsByStatus(
  group: string,
  status: ReactionStatus,
  errorHandler: (error: string) => void,
  cursor?: number,
  limit = RUN_PAGE_SIZE,
): Promise<RunPage | null> {
  return readRunPage(`/reactions/group/${encodeURIComponent(group)}/${encodeURIComponent(status)}/details/`, cursor, errorHandler, limit);
}

/**
 * List the sub reactions a run started (`GET /reactions/sub/{group}/{reaction}/details/`).
 *
 * A workflow run's tool calls run as sub reactions. The listing is followed page by page.
 *
 * @param group - The run's group.
 * @param reaction - The run's reaction id.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The sub reactions read so far; empty if the first request failed.
 */
export async function listSubRuns(group: string, reaction: string, errorHandler: (error: string) => void): Promise<Reaction[]> {
  const runs: Reaction[] = [];
  let cursor: number | undefined;
  // follow the cursor, with a cap so a runaway listing can't loop forever
  for (let page = 0; page < MAX_SUB_RUN_PAGES; page++) {
    const found = await readRunPage(
      `/reactions/sub/${encodeURIComponent(group)}/${encodeURIComponent(reaction)}/details/`,
      cursor,
      errorHandler,
      RUN_PAGE_SIZE,
    );
    if (!found) break;
    runs.push(...found.runs);
    if (found.cursor === null) break;
    cursor = found.cursor;
  }
  return runs;
}

/**
 * Fetch the status updates of a run (`GET /reactions/logs/{group}/{id}`).
 *
 * @param group - The run's group.
 * @param reaction - The run's reaction id.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The status updates; empty if the request failed.
 */
export async function getRunStatusUpdates(group: string, reaction: string, errorHandler: (error: string) => void): Promise<StatusUpdate[]> {
  return client
    .get<StatusUpdate[]>(`/reactions/logs/${encodeURIComponent(group)}/${encodeURIComponent(reaction)}`, { params: { limit: 200 } })
    .then((res) => (res?.status == 200 && Array.isArray(res.data) ? res.data : []))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Run Status Updates');
      return [];
    });
}

/**
 * Fetch the log lines of one stage of a run (`GET /reactions/logs/{group}/{id}/{stage}`).
 *
 * Stage names of tool and workflow runs can hold characters such as `@`, `#`, and `~`, so the
 * name is encoded into the path.
 *
 * @param group - The run's group.
 * @param reaction - The run's reaction id.
 * @param stage - The stage's name from the run's plan.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The log lines (empty if the stage has none yet), or `null` if the request failed.
 */
export async function getRunStageLogs(
  group: string,
  reaction: string,
  stage: string,
  errorHandler: (error: string) => void,
): Promise<string[] | null> {
  return client
    .get<StageLogs>(`/reactions/logs/${encodeURIComponent(group)}/${encodeURIComponent(reaction)}/${encodeURIComponent(stage)}`, {
      params: { limit: 1000 },
    })
    .then((res) => (res?.status == 200 && res.data?.logs ? res.data.logs : []))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Run Stage Logs');
      return null;
    });
}

/**
 * Read the results of a file stored under one tool or workflow result key (`GET /files/results/{sha256}`).
 *
 * @param sha256 - The file's SHA256.
 * @param key - The result key (a tool name, or a workflow run's `name#rN~id` key).
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The results by tool name, or `null` if the request failed.
 */
export async function getSampleRunResults(sha256: string, key: string, errorHandler: (error: string) => void): Promise<RunResults | null> {
  return client
    .get<RunResults>(`/files/results/${encodeURIComponent(sha256)}`, { params: { tools: [key], hidden: true } })
    .then((res) => (res?.status == 200 && res.data ? res.data : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Run Results');
      return null;
    });
}

/**
 * Read the results of a repo stored under one tool or workflow result key (`GET /repos/results/{url}`).
 *
 * @param url - The repo's url.
 * @param key - The result key.
 * @param errorHandler - Called with a formatted message if the request fails.
 * @returns The results by tool name, or `null` if the request failed.
 */
export async function getRepoRunResults(url: string, key: string, errorHandler: (error: string) => void): Promise<RunResults | null> {
  return client
    .get<RunResults>(`/repos/results/${url.split('/').map(encodeURIComponent).join('/')}`, { params: { tools: [key], hidden: true } })
    .then((res) => (res?.status == 200 && res.data ? res.data : null))
    .catch((error: unknown) => {
      parseRequestError(error, errorHandler, 'Get Run Repo Results');
      return null;
    });
}
