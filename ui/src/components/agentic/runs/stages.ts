// project imports
import { DerivedStageState, IndicatorStatus } from './status';
import { JobStatus, Reaction, ReactionStatus, StatusAction, StatusUpdate } from '@models/reactions';
import { RunKind, RunProvenance } from '@models/runs';
import { isRunFinished, recordedStageOrder, runKind } from '@utilities/runs';
import { parseTimestamp } from '@utilities/time';

/** One job of a stage, as the run's status updates tell it. */
export interface StageJob {
  /** The job's id */
  id: string;
  /** The job's last status the updates recorded */
  status: JobStatus;
  /** What the page shows for the job: its status, or Stopped when the run ended while it was queued or running */
  state: IndicatorStatus;
  /** When the job was created */
  created?: string;
  /** When a worker last claimed the job */
  started?: string;
  /** When the job completed or failed */
  finished?: string;
  /** The worker that last claimed the job */
  worker?: string;
  /** The job's deadline */
  deadline?: string;
  /** Whether the job is one of the run's active generators */
  generator: boolean;
  /** How many times the job was reset to be claimed again */
  resets: number;
}

/** One stage of a run with its state and jobs. */
export interface StageView {
  /** The stage's name, which keys its logs and its node */
  name: string;
  /** The stage's step, counted from zero; stages of one step run in parallel */
  step: number;
  /** The tool instance the stage runs, when the run recorded a plan */
  toolInstance?: string;
  /** The stage's state */
  state: IndicatorStatus;
  /** The stage's jobs in creation order */
  jobs: StageJob[];
  /** Whether the stage is in the step the run is working on now */
  active: boolean;
}

/** The parts of a run the stage views read. */
export type StageRun = Pick<Reaction, 'status' | 'current_stage' | 'generators' | 'plan'>;

/**
 * Get the name of a status update's action.
 *
 * @param action - The action as the API sent it; a job reset is an object keyed by `JobReset`.
 * @returns The action's name, such as `JobCreated` or `JobReset`.
 */
export function actionName(action: StatusUpdate['action']): string {
  if (typeof action === 'string') return action;
  return Object.keys(action ?? {})[0] ?? '';
}

/**
 * Get the known action of a status update.
 *
 * @param action - The action as the API sent it.
 * @returns The action, or `null` for one this page does not know.
 */
export function statusAction(action: StatusUpdate['action']): StatusAction | null {
  const name = actionName(action);
  return (Object.values(StatusAction) as string[]).includes(name) ? (name as StatusAction) : null;
}

/** The completed state, typed as a stage state for comparisons. */
const COMPLETED_STATE: IndicatorStatus = JobStatus.Completed;
/** The failed state, typed as a stage state for comparisons. */
const FAILED_STATE: IndicatorStatus = JobStatus.Failed;
/** The stopped state, typed as a stage state for comparisons. */
const STOPPED_STATE: IndicatorStatus = DerivedStageState.Stopped;

/**
 * Turn a stage name into the worker name prefix it matches (lowercase, runs of other characters as one
 * dash), such as `jup-stub-tool` for `jup-stub-tool` or `clamav` for `ClamAV`. The scaler starts a
 * worker's name with its tool's name, so only a stage named after its tool matches its workers.
 *
 * @param stage - The stage's name.
 * @returns The worker name prefix.
 */
export function workerPrefix(stage: string): string {
  return stage
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
}

/**
 * Guess the job a worker claimed from the worker's name, for a claim update that names no job (older
 * status logs name only the worker). The job is the queued job of the stage whose worker name prefix the
 * worker carries (the longest such prefix wins), or the only queued job when no prefix matches, so a guess
 * can miss when parallel stages are not named after their tools.
 *
 * @param worker - The worker's name.
 * @param jobs - The jobs read so far with their stage names.
 * @returns The claimed job, or `undefined` when it can't be told.
 */
export function claimedJob(worker: string, jobs: { stage: string; job: StageJob }[]): StageJob | undefined {
  const queued = jobs.filter(({ job }) => job.status === JobStatus.Created || job.status === JobStatus.Sleeping);
  let best: { length: number; job: StageJob } | undefined;
  for (const { stage, job } of queued) {
    const prefix = workerPrefix(stage);
    const matches = prefix !== '' && (worker === prefix || worker.startsWith(`${prefix}-`));
    // the first queued job of the longest matching prefix wins, so a stage named `a-b` beats `a`
    if (matches && (!best || prefix.length > best.length)) best = { length: prefix.length, job };
  }
  if (best) return best.job;
  return queued.length === 1 ? queued[0].job : undefined;
}

/**
 * Read a status update field, which the API stores JSON encoded for some values.
 *
 * @param value - The stored text, or `undefined`.
 * @returns The text with one layer of JSON string quoting removed.
 */
function field(value: string | undefined): string | undefined {
  if (value === undefined) return undefined;
  return value.length >= 2 && value.startsWith('"') && value.endsWith('"') ? value.slice(1, -1) : value;
}

/**
 * Replay a run's status updates into the jobs of each stage.
 *
 * @param updates - The run's status updates in any order.
 * @param generators - The ids of the run's active generator jobs.
 * @returns The jobs of each stage by stage name, in creation order.
 */
export function stageJobs(updates: StatusUpdate[], generators: string[] = []): Map<string, StageJob[]> {
  const byStage = new Map<string, StageJob[]>();
  const byId = new Map<string, { stage: string; job: StageJob }>();
  // replay in time order; the sort is stable, so updates with one timestamp keep the order they arrived in
  const ordered = [...updates].sort((a, b) => (parseTimestamp(a.timestamp) || 0) - (parseTimestamp(b.timestamp) || 0));
  for (const update of ordered) {
    const values = update.update ?? {};
    const action = statusAction(update.action);
    if (action === StatusAction.JobCreated) {
      const id = values.job ?? values.id;
      const stage = values.stage;
      if (!id || !stage || byId.has(id)) continue;
      const job: StageJob = {
        id,
        status: JobStatus.Created,
        state: JobStatus.Created,
        created: update.timestamp,
        deadline: field(values.deadline),
        generator: generators.includes(id),
        resets: 0,
      };
      byId.set(id, { stage, job });
      byStage.set(stage, [...(byStage.get(stage) ?? []), job]);
      continue;
    }
    // every job update names its job, except a claim in an older status log, which names only its worker
    const guessed = action === StatusAction.JobRunning && !values.job;
    const job = guessed ? claimedJob(values.worker ?? '', [...byId.values()]) : values.job ? byId.get(values.job)?.job : undefined;
    if (!job) continue;
    switch (action) {
      case StatusAction.JobRunning:
        job.status = JobStatus.Running;
        job.started = update.timestamp;
        job.worker = values.worker;
        job.finished = undefined;
        break;
      case StatusAction.JobReset:
        job.status = JobStatus.Created;
        job.resets += 1;
        job.finished = undefined;
        break;
      case StatusAction.JobCompleted:
        job.status = JobStatus.Completed;
        job.finished = update.timestamp;
        break;
      case StatusAction.JobFailed:
        job.status = JobStatus.Failed;
        job.finished = update.timestamp;
        break;
      default:
        break;
    }
  }
  for (const jobs of byStage.values()) for (const job of jobs) job.state = job.status;
  return byStage;
}

/** The job statuses that mean work is still queued or running. */
const UNFINISHED_JOBS: string[] = [JobStatus.Created, JobStatus.Running, JobStatus.Sleeping];

/**
 * Combine the statuses of a stage's jobs into one: a failure wins, then running, sleeping, and queued
 * work; a stage is complete only when every job is.
 *
 * @param jobs - The stage's jobs.
 * @returns The stage's job status.
 */
export function combineJobStatuses(jobs: Pick<StageJob, 'status'>[]): JobStatus {
  for (const status of [JobStatus.Failed, JobStatus.Running, JobStatus.Sleeping, JobStatus.Created]) {
    if (jobs.some((job) => job.status === status)) return status;
  }
  return JobStatus.Completed;
}

/**
 * Work out the state of one stage.
 *
 * @param run - The run.
 * @param step - The stage's step, counted from zero.
 * @param jobs - The stage's jobs.
 * @returns The stage's state.
 */
export function stageState(
  run: Pick<Reaction, 'status' | 'current_stage'>,
  step: number,
  jobs: Pick<StageJob, 'status'>[],
): IndicatorStatus {
  const finished = isRunFinished(run.status);
  if (jobs.length > 0) {
    const combined = combineJobStatuses(jobs);
    // the run moves past a step only once all of its jobs completed
    if (step < run.current_stage && combined !== JobStatus.Failed) return JobStatus.Completed;
    if (finished && UNFINISHED_JOBS.includes(combined))
      return run.status === ReactionStatus.Completed ? JobStatus.Completed : DerivedStageState.Stopped;
    return combined;
  }
  // without jobs in the updates, the run's own position tells the state
  if (step < run.current_stage || run.status === ReactionStatus.Completed) return JobStatus.Completed;
  if (step === run.current_stage) return run.status === ReactionStatus.Failed ? DerivedStageState.Stopped : JobStatus.Created;
  return finished ? DerivedStageState.Skipped : DerivedStageState.Waiting;
}

/**
 * Build the stages of a run with their states and jobs.
 *
 * @param run - The run.
 * @param order - The stage names of each step.
 * @param updates - The run's status updates, or `null` before they are read.
 * @returns The stages of each step, in the order's shape.
 */
export function buildStageViews(run: StageRun, order: string[][], updates: StatusUpdate[] | null): StageView[][] {
  const jobs = stageJobs(updates ?? [], run.generators ?? []);
  const finished = isRunFinished(run.status);
  const tools = new Map((run.plan?.stages ?? []).flat().map((stage) => [stage.name, stage.tool_instance]));
  return order.map((names, step) =>
    names.map((name) => {
      const stageJobList = jobs.get(name) ?? [];
      const state = stageState(run, step, stageJobList);
      // a job left queued or running when the run ended shows as stopped, like its stage
      for (const job of stageJobList) {
        if (finished && UNFINISHED_JOBS.includes(job.status) && state !== COMPLETED_STATE) job.state = DerivedStageState.Stopped;
      }
      return { name, step, toolInstance: tools.get(name), state, jobs: stageJobList, active: !finished && step === run.current_stage };
    }),
  );
}

/**
 * Describe where a run is in its stages, as text beside its status indicator.
 *
 * The API counts `current_stage` from zero and moves it one past the last step once the run completes,
 * so the stage shown is clamped to the plan.
 *
 * @param run - The run.
 * @param steps - The number of steps in the run's order.
 * @returns For example `stage 2 of 5: 1 of 3 jobs done`, `failed at stage 2 of 5`, or `all 5 stages completed`.
 */
export function runProgressText(
  run: Pick<Reaction, 'status' | 'current_stage' | 'current_stage_progress' | 'current_stage_length'>,
  steps: number,
): string {
  const of = steps > 0 ? ` of ${steps}` : '';
  const stage = `stage ${steps > 0 ? Math.min(run.current_stage + 1, steps) : run.current_stage + 1}${of}`;
  switch (run.status) {
    case ReactionStatus.Completed:
      if (steps === 1) return 'the stage completed';
      return steps > 0 ? `all ${steps} stages completed` : 'all stages completed';
    case ReactionStatus.Failed:
      return `failed at ${stage}`;
    case ReactionStatus.Created:
      return `queued at ${stage}`;
    default:
      return `${stage}: ${run.current_stage_progress} of ${run.current_stage_length} ${run.current_stage_length === 1 ? 'job' : 'jobs'} done`;
  }
}

/** What the run page shows for a run's stages. */
export enum RunStagesView {
  /** The stage diagram with each stage's jobs and logs below it */
  Diagram = 'Diagram',
  /** The plan list with logs per stage */
  Plan = 'Plan',
  /** The single stage's jobs and logs, without a diagram or plan */
  SingleStage = 'SingleStage',
}

/**
 * Choose how the run page shows a run's stages.
 *
 * @param run - The run.
 * @returns A diagram for a pipeline run, the stage alone for a run of one tool, and the plan otherwise.
 */
export function runStagesView(run: Pick<Reaction, 'source' | 'plan'> & { args?: Reaction['args'] }): RunStagesView {
  const kind = runKind(run);
  if (kind === RunKind.Pipeline) return RunStagesView.Diagram;
  // a tool run that recorded no plan still has its one stage, named by its arguments
  if (kind === RunKind.Tool && recordedStageOrder(run).flat().length <= 1) return RunStagesView.SingleStage;
  return RunStagesView.Plan;
}

/** Which stages show their logs and which one is selected. */
export interface StageUiState {
  /** The selected stage, or `null` */
  selected: string | null;
  /** The stages whose logs the user opened */
  open: string[];
  /** The stages whose logs the user closed, which overrides opening them by default */
  closed: string[];
}

/** An event that changes a {@link StageUiState}. */
export type StageUiAction = { type: 'select'; stage: string } | { type: 'toggle'; stage: string; shown: boolean };

/** No stage selected and no logs opened or closed. */
export const INITIAL_STAGE_UI: StageUiState = { selected: null, open: [], closed: [] };

/**
 * Advance the stage selection and logs state.
 *
 * @param state - The current state.
 * @param action - Select a stage (which also opens its logs), or toggle a stage's logs from how they show now.
 * @returns The next state.
 */
export function stageUiReducer(state: StageUiState, action: StageUiAction): StageUiState {
  const without = (names: string[]) => names.filter((name) => name !== action.stage);
  switch (action.type) {
    case 'select':
      return { selected: action.stage, open: [...without(state.open), action.stage], closed: without(state.closed) };
    case 'toggle':
      // hiding wins over a default-open stage, and showing again undoes it
      return action.shown
        ? { ...state, open: without(state.open), closed: [...without(state.closed), action.stage] }
        : { ...state, open: [...without(state.open), action.stage], closed: without(state.closed) };
    default:
      return state;
  }
}

/**
 * Tell whether a stage's logs show.
 *
 * @param state - The selection and logs state.
 * @param stage - The stage's name.
 * @param defaultOpen - Whether the stage opens its logs unless the user closed them.
 * @returns Whether the logs show.
 */
export function logsShown(state: StageUiState, stage: string, defaultOpen: boolean): boolean {
  return state.open.includes(stage) || (defaultOpen && !state.closed.includes(stage));
}

/**
 * Tell whether a stage opens its logs by default.
 *
 * @param view - How the page shows the run's stages.
 * @param run - The run.
 * @param stage - The stage.
 * @returns True for a run of one stage, and for the failed or stopped stages of a failed run, so a failure is visible at once.
 */
export function logsOpenByDefault(view: RunStagesView, run: Pick<Reaction, 'status'>, stage: Pick<StageView, 'state'>): boolean {
  if (view === RunStagesView.SingleStage) return true;
  return run.status === ReactionStatus.Failed && (stage.state === FAILED_STATE || stage.state === STOPPED_STATE);
}

/** The input rows a run summary shows. */
export enum InputRow {
  /** The input files */
  Samples = 'Samples',
  /** The input repos */
  Repos = 'Repos',
  /** The run had no inputs at all */
  None = 'None',
}

/**
 * Choose the input rows of a run summary: only the kinds of input the run had, or a single note when it had none.
 *
 * @param run - The run.
 * @returns The rows to show.
 */
export function inputRows(run: Pick<Reaction, 'samples' | 'repos'>): InputRow[] {
  const rows: InputRow[] = [];
  if ((run.samples ?? []).length > 0) rows.push(InputRow.Samples);
  if ((run.repos ?? []).length > 0) rows.push(InputRow.Repos);
  return rows.length > 0 ? rows : [InputRow.None];
}

/**
 * Write the intro of a run's Versions used section, naming only the kinds of version its tables list.
 *
 * @param provenance - The run's provenance, or `null`.
 * @param harness - The harness tool instance of a workflow run.
 * @returns For example `The exact agent, harness and tool versions this run used.`, or `null` when none were recorded.
 */
export function versionsIntro(provenance: RunProvenance | null, harness?: string): string | null {
  if (!provenance) return null;
  const kinds: string[] = [];
  if (provenance.agents.length > 0) kinds.push('agent');
  if (harness && provenance.tools.some((tool) => tool.tool_instance === harness)) kinds.push('harness');
  if (provenance.tools.some((tool) => tool.tool_instance !== harness)) kinds.push('tool');
  if (kinds.length === 0) return null;
  const list = kinds.length === 1 ? kinds[0] : `${kinds.slice(0, -1).join(', ')} and ${kinds[kinds.length - 1]}`;
  return `The exact ${list} versions this run used.`;
}
