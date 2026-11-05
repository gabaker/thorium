import { describe, expect, it } from 'vitest';

// project imports
import {
  actionName,
  buildStageViews,
  claimedJob,
  combineJobStatuses,
  INITIAL_STAGE_UI,
  InputRow,
  inputRows,
  jobsLiveAfterFailure,
  liveAfterFailure,
  logsOpenByDefault,
  logsShown,
  RunStagesView,
  runProgressText,
  runStagesView,
  StageJob,
  stageJobs,
  stageUiReducer,
  versionsIntro,
  workerPrefix,
} from './stages';
import { JobStatus, Reaction, ReactionStatus, StatusUpdate } from '@models/reactions';
import { RunProvenance } from '@models/runs';

/** A status update at a second offset from a fixed start. */
function update(action: StatusUpdate['action'], values: Record<string, string>, second = 0): StatusUpdate {
  return {
    group: 'g',
    pipeline: 'p',
    reaction: 'r',
    action,
    timestamp: new Date(Date.parse('2026-10-29T10:00:00Z') + second * 1000).toISOString(),
    update: values,
  };
}

/** A job creation update. */
const created = (job: string, stage: string, second = 0) =>
  update('JobCreated', { job, id: job, stage, status: '"Created"', deadline: '2026-11-05 20:25:56.1 UTC' }, second);
/** A job claim update from an older status log, which names only the worker. */
const claimed = (worker: string, second = 0) => update('JobRunning', { worker, status: 'running' }, second);
/** A job claim update that names the claimed job and its stage beside the worker. */
const claimedJobUpdate = (job: string, stage: string, worker: string, second = 0) =>
  update('JobRunning', { job, stage, worker, status: 'running' }, second);
/** A job completion update. */
const completed = (job: string, second = 0) => update('JobCompleted', { job, status: 'Completed' }, second);
/** A job failure update. */
const failed = (job: string, second = 0) => update('JobFailed', { job, status: 'Failed' }, second);

/** A three step pipeline run: scan, then two stages in parallel, then report. */
const ORDER = [['scan'], ['strings', 'yara'], ['report']];

/** A run at a status and stage. */
function run(status: ReactionStatus, current_stage: number, overrides: Partial<Reaction> = {}): Reaction {
  return {
    id: 'r',
    group: 'g',
    creator: 'test',
    pipeline: 'p',
    status,
    current_stage,
    current_stage_progress: 0,
    current_stage_length: 1,
    args: {},
    sla: '2026-11-05T20:26:06Z',
    jobs: [],
    tags: [],
    sub_reactions: 0,
    completed_sub_reactions: 0,
    generators: [],
    samples: [],
    ephemeral: [],
    parent_ephemeral: {},
    repos: [],
    has_cache: false,
    ...overrides,
  };
}

/** The state of each stage by name. */
const states = (views: ReturnType<typeof buildStageViews>) => Object.fromEntries(views.flat().map((stage) => [stage.name, stage.state]));

describe('status update actions', () => {
  it('names a job reset sent as an object', () => {
    expect(actionName('JobCreated')).toBe('JobCreated');
    expect(actionName({ JobReset: { Component: 'Scaler' } })).toBe('JobReset');
  });
});

describe('claimedJob', () => {
  const job = (id: string): StageJob => ({ id, status: JobStatus.Created, state: JobStatus.Created, generator: false, resets: 0 });
  it('turns a stage name into the worker name prefix', () => {
    expect(workerPrefix('jup-stub-tool')).toBe('jup-stub-tool');
    expect(workerPrefix('My_Tool v1')).toBe('my-tool-v1');
  });
  it('picks the queued job whose stage prefixes the worker, longest prefix first', () => {
    const jobs = [
      { stage: 'strings', job: job('a') },
      { stage: 'strings-extra', job: job('b') },
    ];
    expect(claimedJob('strings-extra-1-0-0-x1', jobs)?.id).toBe('b');
    expect(claimedJob('strings-latest-x2', jobs)?.id).toBe('a');
  });
  it('falls back to the only queued job, and gives up when it cannot tell', () => {
    expect(claimedJob('w-123', [{ stage: 'scan', job: job('a') }])?.id).toBe('a');
    expect(
      claimedJob('w-123', [
        { stage: 'scan', job: job('a') },
        { stage: 'yara', job: job('b') },
      ]),
    ).toBeUndefined();
  });
});

describe('stageJobs', () => {
  it('replays a job from creation through its claim to completion', () => {
    const jobs = stageJobs([created('j1', 'scan', 0), claimed('scan-latest-abc', 5), completed('j1', 9)]).get('scan')!;
    expect(jobs).toHaveLength(1);
    expect(jobs[0]).toMatchObject({
      id: 'j1',
      status: JobStatus.Completed,
      worker: 'scan-latest-abc',
      deadline: '2026-11-05 20:25:56.1 UTC',
    });
    expect(jobs[0].created).toBeDefined();
    expect(jobs[0].started).toBeDefined();
    expect(jobs[0].finished).toBeDefined();
  });
  it('counts a reset and puts the job back in the queue', () => {
    const jobs = stageJobs([
      created('j1', 'scan', 0),
      claimed('scan-x', 1),
      update({ JobReset: 'User' }, { job: 'j1', status: 'Created' }, 2),
    ]).get('scan')!;
    expect(jobs[0]).toMatchObject({ status: JobStatus.Created, resets: 1 });
  });
  it('replays updates in time order whatever order they arrive in, and marks generators', () => {
    const jobs = stageJobs([failed('j1', 9), created('j1', 'scan', 0)], ['j1']).get('scan')!;
    expect(jobs[0]).toMatchObject({ status: JobStatus.Failed, generator: true });
  });
  it('trusts the job a claim names over a worker name that prefixes another stage', () => {
    // the worker runs the tool jup-stub-tool for the stage stub-stage, and its name starts with the stage name jup
    const queued = [created('j1', 'jup', 0), created('j2', 'stub-stage', 0)];
    const named = stageJobs([...queued, claimedJobUpdate('j2', 'stub-stage', 'jup-stub-tool-1-0-0-x', 1)]);
    expect(named.get('stub-stage')![0]).toMatchObject({ status: JobStatus.Running, worker: 'jup-stub-tool-1-0-0-x' });
    expect(named.get('jup')![0].status).toBe(JobStatus.Created);
    // an older claim naming only the worker is guessed by the name, which picks the wrong stage
    const guessed = stageJobs([...queued, claimed('jup-stub-tool-1-0-0-x', 1)]);
    expect(guessed.get('jup')![0].status).toBe(JobStatus.Running);
    expect(guessed.get('stub-stage')![0].status).toBe(JobStatus.Created);
  });
  it('replays a log that mixes claims with and without job ids', () => {
    const jobs = stageJobs([
      created('j1', 'scan', 0),
      claimed('scan-1-0-0-a', 1),
      completed('j1', 2),
      created('j2', 'strings', 3),
      created('j3', 'yara', 3),
      claimedJobUpdate('j3', 'yara', 'tool-yara-1-0-0-b', 4),
      claimedJobUpdate('j2', 'strings', 'tool-strings-1-0-0-c', 5),
      completed('j3', 6),
    ]);
    expect(jobs.get('scan')![0]).toMatchObject({ status: JobStatus.Completed, worker: 'scan-1-0-0-a' });
    expect(jobs.get('strings')![0]).toMatchObject({ status: JobStatus.Running, worker: 'tool-strings-1-0-0-c' });
    expect(jobs.get('yara')![0]).toMatchObject({ status: JobStatus.Completed, worker: 'tool-yara-1-0-0-b' });
  });
  it('skips a claim naming a job the log never created instead of guessing', () => {
    const [job] = stageJobs([created('j1', 'scan', 0), claimedJobUpdate('j9', 'scan', 'scan-1-0-0-a', 1)]).get('scan')!;
    expect(job.status).toBe(JobStatus.Created);
    expect(job.worker).toBeUndefined();
  });
});

describe('combineJobStatuses', () => {
  it('lets a failure win, then running, sleeping, and queued work', () => {
    expect(combineJobStatuses([{ status: JobStatus.Completed }, { status: JobStatus.Failed }])).toBe(JobStatus.Failed);
    expect(combineJobStatuses([{ status: JobStatus.Completed }, { status: JobStatus.Running }])).toBe(JobStatus.Running);
    expect(combineJobStatuses([{ status: JobStatus.Sleeping }, { status: JobStatus.Created }])).toBe(JobStatus.Sleeping);
    expect(combineJobStatuses([{ status: JobStatus.Completed }])).toBe(JobStatus.Completed);
  });
});

describe('buildStageViews', () => {
  it('builds a node per stage in the order shape, with the plan tool and the running step active', () => {
    const plan = {
      stages: [
        [{ name: 'scan', tool_instance: 't1' }],
        [
          { name: 'strings', tool_instance: 't2' },
          { name: 'yara', tool_instance: 't3' },
        ],
        [{ name: 'report', tool_instance: 't4' }],
      ],
    };
    const views = buildStageViews(run(ReactionStatus.Started, 1, { plan }), ORDER, [
      created('j1', 'scan'),
      completed('j1', 2),
      created('j2', 'strings', 3),
      created('j3', 'yara', 3),
      claimed('yara-1-0-x', 4),
    ]);
    expect(views.map((step) => step.map((stage) => stage.name))).toEqual(ORDER);
    expect(views[1][1].toolInstance).toBe('t3');
    expect(states(views)).toEqual({ scan: 'Completed', strings: 'Created', yara: 'Running', report: 'Waiting' });
    expect(
      views
        .flat()
        .filter((stage) => stage.active)
        .map((stage) => stage.name),
    ).toEqual(['strings', 'yara']);
  });
  it('shows a stage running once its claim names the job, though its tool is named otherwise', () => {
    // the parallel stages run tools named tool-strings and tool-yara, so their workers match neither stage name
    const queued = [created('j1', 'scan'), completed('j1', 2), created('j2', 'strings', 3), created('j3', 'yara', 3)];
    const named = buildStageViews(run(ReactionStatus.Started, 1), ORDER, [
      ...queued,
      claimedJobUpdate('j3', 'yara', 'tool-yara-1-0-0-b', 4),
    ]);
    expect(states(named)).toEqual({ scan: 'Completed', strings: 'Created', yara: 'Running', report: 'Waiting' });
    // an older claim naming only the worker can't be told apart from the other queued job, so both stay queued
    const guessed = buildStageViews(run(ReactionStatus.Started, 1), ORDER, [...queued, claimed('tool-yara-1-0-0-b', 4)]);
    expect(states(guessed)).toEqual({ scan: 'Completed', strings: 'Created', yara: 'Created', report: 'Waiting' });
  });
  it('shows a completed run as all completed, even with no updates read', () => {
    expect(states(buildStageViews(run(ReactionStatus.Completed, 3), ORDER, null))).toEqual({
      scan: 'Completed',
      strings: 'Completed',
      yara: 'Completed',
      report: 'Completed',
    });
  });
  it('shows a failed run: the failed stage, its still running neighbour, and the skipped stages after it', () => {
    const views = buildStageViews(run(ReactionStatus.Failed, 1), ORDER, [
      created('j1', 'scan'),
      completed('j1', 1),
      created('j2', 'strings', 2),
      created('j3', 'yara', 2),
      claimed('yara-x', 3),
      failed('j2', 4),
    ]);
    expect(states(views)).toEqual({ scan: 'Completed', strings: 'Failed', yara: 'Running', report: 'Skipped' });
    // the job left running when the run failed keeps running, with a note saying the run failed
    expect(views[1][1].jobs[0]).toMatchObject({ status: JobStatus.Running, state: 'Running' });
    expect(views[1][1].jobs[0].note).toMatch(/run failed.*still running/);
    expect(views[1][1].note).toMatch(/run failed.*still running/);
    expect(views[1][0].note).toBeUndefined();
    expect(views.flat().some((stage) => stage.active)).toBe(false);
  });
  it('shows a cancelled run, which fails with no job failure, as running its claimed job and stopped with a run token', () => {
    const updates = [created('j1', 'scan'), claimed('scan-x', 1)];
    expect(states(buildStageViews(run(ReactionStatus.Failed, 0), [['scan'], ['report']], updates))).toEqual({
      scan: 'Running',
      report: 'Skipped',
    });
    // a run token is revoked with its run, so its worker can never report back
    const tokenRun = run(ReactionStatus.Failed, 0, { id: 'r', run: 'r', run_token: 'run-r' });
    const views = buildStageViews(tokenRun, [['scan'], ['report']], updates);
    expect(states(views)).toEqual({ scan: 'Stopped', report: 'Skipped' });
    expect(views[0][0].jobs[0]).toMatchObject({ status: JobStatus.Running, state: 'Stopped' });
    expect(views[0][0].jobs[0].note).toBeUndefined();
  });
  it('follows the polls of a running run: queued, running, then the next step', () => {
    const order = [['scan'], ['report']];
    const polls: [Reaction, StatusUpdate[]][] = [
      [run(ReactionStatus.Created, 0), [created('j1', 'scan')]],
      [run(ReactionStatus.Started, 0), [created('j1', 'scan'), claimed('scan-x', 1)]],
      [run(ReactionStatus.Started, 1), [created('j1', 'scan'), claimed('scan-x', 1), completed('j1', 2), created('j2', 'report', 3)]],
      [
        run(ReactionStatus.Completed, 2),
        [
          created('j1', 'scan'),
          claimed('scan-x', 1),
          completed('j1', 2),
          created('j2', 'report', 3),
          claimed('report-x', 4),
          completed('j2', 5),
        ],
      ],
    ];
    expect(polls.map(([current, updates]) => states(buildStageViews(current, order, updates)))).toEqual([
      { scan: 'Created', report: 'Waiting' },
      { scan: 'Running', report: 'Waiting' },
      { scan: 'Completed', report: 'Created' },
      { scan: 'Completed', report: 'Completed' },
    ]);
  });
  it('lists many parallel stages without losing any', () => {
    const wide = [Array.from({ length: 40 }, (_, index) => `tool-${index}`)];
    const views = buildStageViews(
      run(ReactionStatus.Started, 0),
      wide,
      wide[0].map((name, index) => created(`j${index}`, name)),
    );
    expect(views[0]).toHaveLength(40);
    expect(new Set(views[0].map((stage) => stage.state))).toEqual(new Set(['Created']));
  });
});

/**
 * Status updates recorded by the API for a pipeline run of two parallel stages, `alpha` and `beta`, then
 * `gamma`, where `alpha`'s job errors (ids and names shortened, fields as the API wrote them).
 */
const PARALLEL_ORDER = [['alpha', 'beta'], ['gamma']];
/** The deadline the API wrote on both jobs. */
const DEADLINE = '2026-11-11 23:43:00.709803282 UTC';
/** A job creation update as the API writes it. */
const apiCreated = (job: string, stage: string, timestamp: string) => ({
  ...update('JobCreated', { deadline: DEADLINE, stage, job, id: job, reaction: 'r', status: '"Created"' }),
  timestamp,
});
/** A job update as the API writes it. */
const apiJob = (action: string, values: Record<string, string>, timestamp: string) => ({ ...update(action, values), timestamp });
/** Both jobs claimed, then `alpha`'s job errors, which fails the run. */
const BOTH_CLAIMED: StatusUpdate[] = [
  apiJob('ReactionCreated', { status: '"Created"', current_stage: '0' }, '2026-11-05T00:03:00.709874843Z'),
  apiCreated('ja', 'alpha', '2026-11-05T00:03:00.710752917Z'),
  apiCreated('jb', 'beta', '2026-11-05T00:03:00.710870698Z'),
  apiJob('JobRunning', { status: 'running', worker: 'wa', stage: 'alpha', job: 'ja' }, '2026-11-05T00:03:00.744291876Z'),
  apiJob('JobRunning', { status: 'running', worker: 'wb', stage: 'beta', job: 'jb' }, '2026-11-05T00:03:00.751352770Z'),
  apiJob('JobFailed', { job: 'ja', status: 'Failed' }, '2026-11-05T00:03:00.798318056Z'),
  apiJob('ReactionFailed', { status: 'Failed' }, '2026-11-05T00:03:00.800749032Z'),
];
/** `beta`'s job completing after the run failed, which the API accepts and records. */
const BETA_COMPLETED = apiJob('JobCompleted', { job: 'jb', status: 'Completed' }, '2026-11-05T00:03:00.837769860Z');
/** Only `alpha`'s job claimed, then it errors while `beta`'s job is still queued. */
const ONE_CLAIMED: StatusUpdate[] = [
  apiJob('ReactionCreated', { status: '"Created"', current_stage: '0' }, '2026-11-05T00:03:00.881445338Z'),
  apiCreated('ja', 'alpha', '2026-11-05T00:03:00.882376308Z'),
  apiCreated('jb', 'beta', '2026-11-05T00:03:00.882495626Z'),
  apiJob('JobRunning', { job: 'ja', status: 'running', worker: 'wa', stage: 'alpha' }, '2026-11-05T00:03:00.889058467Z'),
  apiJob('JobFailed', { job: 'ja', status: 'Failed' }, '2026-11-05T00:03:00.927389746Z'),
  apiJob('ReactionFailed', { status: 'Failed' }, '2026-11-05T00:03:00.929737144Z'),
];
/** A worker claiming and completing the queued `beta` job after the run failed, which the API still allows. */
const BETA_CLAIMED_LATE = [
  apiJob('JobRunning', { status: 'running', worker: 'wb', job: 'jb', stage: 'beta' }, '2026-11-05T00:03:00.962266139Z'),
  apiJob('JobCompleted', { job: 'jb', status: 'Completed' }, '2026-11-05T00:03:01.000313352Z'),
];

describe('a parallel stage failing in a pipeline run', () => {
  /** The failed pipeline run, at its first step. */
  const failedRun = run(ReactionStatus.Failed, 0, { current_stage_length: 2 });
  /** The state and note of each job by id. */
  const jobStates = (views: ReturnType<typeof buildStageViews>) =>
    Object.fromEntries(views.flat().flatMap((stage) => stage.jobs.map((job) => [job.id, { state: job.state, note: job.note }])));
  it('keeps the running sibling running with a note, then shows its final state', () => {
    const running = buildStageViews(failedRun, PARALLEL_ORDER, BOTH_CLAIMED);
    expect(states(running)).toEqual({ alpha: 'Failed', beta: 'Running', gamma: 'Skipped' });
    expect(jobStates(running).ja).toEqual({ state: 'Failed', note: undefined });
    expect(jobStates(running).jb.state).toBe('Running');
    expect(jobStates(running).jb.note).toMatch(/still running/);
    const done = buildStageViews(failedRun, PARALLEL_ORDER, [...BOTH_CLAIMED, BETA_COMPLETED]);
    expect(states(done)).toEqual({ alpha: 'Failed', beta: 'Completed', gamma: 'Skipped' });
    expect(jobStates(done).jb).toEqual({ state: 'Completed', note: undefined });
    expect(done[0][1].note).toBeUndefined();
  });
  it('keeps the queued sibling queued with a note, since a worker can still claim it, then follows it', () => {
    const queued = buildStageViews(failedRun, PARALLEL_ORDER, ONE_CLAIMED);
    expect(states(queued)).toEqual({ alpha: 'Failed', beta: 'Created', gamma: 'Skipped' });
    expect(jobStates(queued).jb.note).toMatch(/still queued/);
    expect(queued[0][1].note).toMatch(/still queued/);
    const claimedLate = buildStageViews(failedRun, PARALLEL_ORDER, [...ONE_CLAIMED, BETA_CLAIMED_LATE[0]]);
    expect(states(claimedLate)).toEqual({ alpha: 'Failed', beta: 'Running', gamma: 'Skipped' });
    const done = buildStageViews(failedRun, PARALLEL_ORDER, [...ONE_CLAIMED, ...BETA_CLAIMED_LATE]);
    expect(states(done)).toEqual({ alpha: 'Failed', beta: 'Completed', gamma: 'Skipped' });
  });
  it('stops the unfinished siblings of a run with a run token, whose queued jobs are removed and token revoked', () => {
    const tokenRun = { ...failedRun, run: 'r', run_token: 'run-r' };
    expect(states(buildStageViews(tokenRun, PARALLEL_ORDER, BOTH_CLAIMED))).toEqual({ alpha: 'Failed', beta: 'Stopped', gamma: 'Skipped' });
    expect(states(buildStageViews(tokenRun, PARALLEL_ORDER, ONE_CLAIMED))).toEqual({ alpha: 'Failed', beta: 'Stopped', gamma: 'Skipped' });
  });
  it('stops a sleeping generator of a failed run, whose last update is still its claim', () => {
    // the API records nothing when a generator sleeps, so only the run's generators list tells it apart
    const sleeping = { ...failedRun, generators: ['jb'] };
    const views = buildStageViews(sleeping, PARALLEL_ORDER, BOTH_CLAIMED);
    expect(states(views)).toEqual({ alpha: 'Failed', beta: 'Stopped', gamma: 'Skipped' });
    expect(jobStates(views).jb).toEqual({ state: 'Stopped', note: undefined });
    expect(views[0][1].note).toBeUndefined();
    expect(jobsLiveAfterFailure(sleeping, BOTH_CLAIMED, Date.parse('2026-11-05T00:05:00Z'))).toBe(false);
  });
  it('gives a stage whose jobs can still run the deadline its logs are read until', () => {
    const running = buildStageViews(failedRun, PARALLEL_ORDER, BOTH_CLAIMED);
    expect(running[0][1].liveUntil).toBe(Date.parse('2026-11-11T23:43:00.709Z'));
    expect(running[0][0].liveUntil).toBeUndefined();
    const done = buildStageViews(failedRun, PARALLEL_ORDER, [...BOTH_CLAIMED, BETA_COMPLETED]);
    expect(done[0][1].liveUntil).toBeUndefined();
  });
  it('tells which jobs can still run after their run failed', () => {
    expect(liveAfterFailure(failedRun, JobStatus.Running)).toBe(true);
    expect(liveAfterFailure(failedRun, JobStatus.Created)).toBe(true);
    // a sleeping generator is not respawned for a failed run
    expect(liveAfterFailure(failedRun, JobStatus.Sleeping)).toBe(false);
    expect(liveAfterFailure(failedRun, JobStatus.Completed)).toBe(false);
    expect(liveAfterFailure({ ...failedRun, run: 'r' }, JobStatus.Running)).toBe(false);
    expect(liveAfterFailure(run(ReactionStatus.Started, 0), JobStatus.Running)).toBe(false);
  });
  it('keeps reading the updates of a failed run while a job can still run before its deadline', () => {
    const now = Date.parse('2026-11-05T00:05:00Z');
    expect(jobsLiveAfterFailure(failedRun, BOTH_CLAIMED, now)).toBe(true);
    expect(jobsLiveAfterFailure(failedRun, ONE_CLAIMED, now)).toBe(true);
    expect(jobsLiveAfterFailure(failedRun, [...BOTH_CLAIMED, BETA_COMPLETED], now)).toBe(false);
    expect(jobsLiveAfterFailure(failedRun, BOTH_CLAIMED, Date.parse('2026-11-12T00:00:00Z'))).toBe(false);
    expect(jobsLiveAfterFailure({ ...failedRun, run: 'r' }, BOTH_CLAIMED, now)).toBe(false);
    expect(jobsLiveAfterFailure(failedRun, undefined, now)).toBe(false);
  });
});

describe('runProgressText', () => {
  const base = { current_stage_progress: 1, current_stage_length: 3 };
  it('counts stages from one while the run works', () => {
    expect(runProgressText({ ...base, status: ReactionStatus.Started, current_stage: 1 }, 5)).toBe('stage 2 of 5: 1 of 3 jobs done');
    expect(runProgressText({ ...base, status: ReactionStatus.Created, current_stage: 0 }, 5)).toBe('queued at stage 1 of 5');
  });
  it('reads a completed run as done, though the API moves its stage past the last one', () => {
    expect(runProgressText({ ...base, status: ReactionStatus.Completed, current_stage: 5 }, 5)).toBe('all 5 stages completed');
    expect(runProgressText({ ...base, status: ReactionStatus.Completed, current_stage: 1 }, 1)).toBe('the stage completed');
  });
  it('names the stage a failed run stopped at, clamped to the plan', () => {
    expect(runProgressText({ ...base, status: ReactionStatus.Failed, current_stage: 9 }, 2)).toBe('failed at stage 2 of 2');
    expect(runProgressText({ ...base, status: ReactionStatus.Started, current_stage: 0 }, 0)).toBe('stage 1: 1 of 3 jobs done');
  });
});

describe('runStagesView', () => {
  const provenance = { agents: [], tools: [] };
  const stage = (name: string) => ({ name, tool_instance: 'i' });
  it('shows a pipeline run as a diagram', () => {
    expect(runStagesView({ source: { Pipeline: { name: 'p', provenance } }, plan: { stages: [[stage('a')]] } })).toBe(
      RunStagesView.Diagram,
    );
    expect(runStagesView({})).toBe(RunStagesView.Diagram);
  });
  it('keeps the plan for a workflow run and a run of several tools', () => {
    const workflow = {
      WorkflowRun: { workflow_instance: 'w', digest: 'd', agent: 'a', harness: 'h', result_key: 'k', models: [], provenance },
    };
    expect(runStagesView({ source: workflow, plan: { stages: [[stage('a')]] } })).toBe(RunStagesView.Plan);
    expect(runStagesView({ source: { Tool: { tool_instance: 'i', provenance } }, plan: { stages: [[stage('a')], [stage('b')]] } })).toBe(
      RunStagesView.Plan,
    );
  });
  it('shows a run of one tool as its stage alone, so its logs stay on the page', () => {
    expect(runStagesView({ source: { Tool: { tool_instance: 'i', provenance } }, plan: { stages: [[stage('a')]] } })).toBe(
      RunStagesView.SingleStage,
    );
  });
  it('shows a tool run that recorded no plan as its stage alone instead of an empty plan', () => {
    expect(runStagesView({ source: { Tool: { tool_instance: 'i', provenance } }, args: { scan: {} } as never })).toBe(
      RunStagesView.SingleStage,
    );
    expect(runStagesView({ source: { Tool: { tool_instance: 'i', provenance } } })).toBe(RunStagesView.SingleStage);
  });
});

describe('stage selection', () => {
  it('selects a stage and opens its logs', () => {
    const state = stageUiReducer(INITIAL_STAGE_UI, { type: 'select', stage: 'yara' });
    expect(state.selected).toBe('yara');
    expect(logsShown(state, 'yara', false)).toBe(true);
    expect(logsShown(state, 'scan', false)).toBe(false);
  });
  it('lets hiding win over a default-open stage, and selecting reopen it', () => {
    let state = stageUiReducer(INITIAL_STAGE_UI, { type: 'toggle', stage: 'scan', shown: true });
    expect(logsShown(state, 'scan', true)).toBe(false);
    state = stageUiReducer(state, { type: 'select', stage: 'scan' });
    expect(logsShown(state, 'scan', true)).toBe(true);
    state = stageUiReducer(state, { type: 'toggle', stage: 'scan', shown: true });
    expect(logsShown(state, 'scan', false)).toBe(false);
  });
  it('opens the logs of a single stage run and of the failed or stopped stages of a failed run', () => {
    expect(logsOpenByDefault(RunStagesView.SingleStage, { status: ReactionStatus.Completed }, { state: 'Completed' })).toBe(true);
    expect(logsOpenByDefault(RunStagesView.Diagram, { status: ReactionStatus.Failed }, { state: 'Failed' })).toBe(true);
    expect(logsOpenByDefault(RunStagesView.Diagram, { status: ReactionStatus.Failed }, { state: 'Stopped' })).toBe(true);
    expect(logsOpenByDefault(RunStagesView.Diagram, { status: ReactionStatus.Failed }, { state: 'Completed' })).toBe(false);
    expect(logsOpenByDefault(RunStagesView.Diagram, { status: ReactionStatus.Started }, { state: 'Running' })).toBe(false);
  });
});

describe('inputRows', () => {
  const sha = 'a'.repeat(64);
  it('shows only the input files when the run had no repos', () => {
    expect(inputRows({ samples: [sha], repos: [] })).toEqual([InputRow.Samples]);
  });
  it('shows only the input repos when the run had no files', () => {
    expect(inputRows({ samples: [], repos: [{ url: 'github.com/a/b' }] })).toEqual([InputRow.Repos]);
  });
  it('shows both when the run had both', () => {
    expect(inputRows({ samples: [sha], repos: [{ url: 'github.com/a/b' }] })).toEqual([InputRow.Samples, InputRow.Repos]);
  });
  it('says No inputs only when the run had none at all', () => {
    expect(inputRows({ samples: [], repos: [] })).toEqual([InputRow.None]);
  });
});

describe('versionsIntro', () => {
  const tool = (id: string) =>
    ({ tool: 't', tool_instance: id, name: id, kind: 'Image', version: '1', spec_hash: 'h' }) as RunProvenance['tools'][number];
  const agent = { agent: 'a', name: 'a', config_hash: 'c', toolset_digest: 'd' };
  it('names only tools for a pipeline or tool run', () => {
    expect(versionsIntro({ agents: [], tools: [tool('x')] })).toBe('The exact tool versions this run used.');
  });
  it('names agents, the harness, and tools for a workflow run that called tools', () => {
    expect(versionsIntro({ agents: [agent], tools: [tool('h'), tool('x')] }, 'h')).toBe(
      'The exact agent, harness and tool versions this run used.',
    );
    expect(versionsIntro({ agents: [agent], tools: [tool('h')] }, 'h')).toBe('The exact agent and harness versions this run used.');
  });
  it('has no intro when nothing was recorded', () => {
    expect(versionsIntro(null)).toBeNull();
    expect(versionsIntro({ agents: [], tools: [] })).toBeNull();
  });
});
