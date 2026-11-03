import { describe, expect, it } from 'vitest';

// project imports
import { BLANK_INPUTS, InputsMode, InputsRequirement, inputsSummary, visibleInputs } from './inputs';
import { LaunchTab } from './launch';
import {
  BLANK_LAUNCH_STATE,
  countByKind,
  describeItem,
  guardedLaunch,
  initialLaunchState,
  itemSettings,
  itemSubmission,
  LaunchAction,
  LaunchActionType,
  LaunchCatalog,
  launchBlocker,
  launchItemKey,
  launchLabel,
  launchReducer,
  launchRequirement,
  launchRunPath,
  launchRuns,
  launchSearchParams,
  LaunchState,
  LaunchSubmission,
  mapWithConcurrency,
  newLaunchItem,
  uploadGroups,
} from './launchForms';
import { Pipeline } from '@models/pipelines';
import { Tool } from '@models/tools';
import { Workflow } from '@models/workflows';

/** Two well-formed SHA256s. */
const A = 'a'.repeat(64);
const B = 'b'.repeat(64);

/**
 * Build a tool as the list serves it.
 *
 * @param id - The tool's id.
 * @param name - The tool's name.
 * @param groups - The tool's groups.
 * @param kind - The tool kind.
 * @returns The tool.
 */
function tool(id: string, name: string, groups = ['system'], kind = 'Image'): Tool {
  return {
    id,
    name,
    groups,
    metadata: { Tool: { kind, latest: `${id}-v2`, instances: [{ id: `${id}-v1` }, { id: `${id}-v2` }] } },
  } as unknown as Tool;
}

/** A workflow in `system` whose latest instance is `wi`. */
const WORKFLOW = {
  id: 'w',
  name: 'triage',
  groups: ['system'],
  metadata: { WorkflowCatalog: { latest_instance: 'wi' } },
} as unknown as Workflow;

/** A runnable pipeline and one with a legacy stage. */
const PIPELINE = { group: 'system', name: 'scan-all', order: [['scan']], stage_tools: { scan: { tool: 't1' } } } as unknown as Pipeline;
const LEGACY = { group: 'system', name: 'legacy', order: [['harvest']], stage_tools: {} } as unknown as Pipeline;

/** Everything listed and nothing loading. */
const CATALOG: LaunchCatalog = {
  workflows: [WORKFLOW],
  tools: [tool('t1', 'scan'), tool('t2', 'strings'), tool('h', 'harness', ['system'], 'Harness'), tool('m', 'multi', ['system', 'other'])],
  pipelines: [PIPELINE, LEGACY],
  instances: {},
  loading: [],
};

/** The signed in user's groups. */
const USER_GROUPS = ['system', 'other'];

/**
 * Apply a list of changes to the blank state.
 *
 * @param actions - The changes, in order.
 * @returns The state after them.
 */
function apply(...actions: LaunchAction[]): LaunchState {
  return actions.reduce(launchReducer, BLANK_LAUNCH_STATE);
}

/** Two tools and a pipeline chosen across the tabs, with a sample entered. */
function threeRuns(): LaunchState {
  return apply(
    { type: LaunchActionType.Select, kind: LaunchTab.Tool, refs: ['t1', 't2'], latest: { t1: 't1-v2', t2: 't2-v2' } },
    { type: LaunchActionType.Select, kind: LaunchTab.Pipeline, refs: ['system/scan-all'] },
    { type: LaunchActionType.Inputs, inputs: { ...BLANK_INPUTS, samples: [A] } },
  );
}

describe('launch reducer', () => {
  it('adds chosen runs of one kind after the others, keeping the options of runs that stay', () => {
    let state = threeRuns();
    state = launchReducer(state, { type: LaunchActionType.Item, key: 'tool:t1', patch: { tags: 'mine' } });
    state = launchReducer(state, { type: LaunchActionType.Select, kind: LaunchTab.Tool, refs: ['t1'] });
    expect(state.items.map(launchItemKey)).toEqual(['tool:t1', 'pipeline:system/scan-all']);
    expect(state.items[0]).toMatchObject({ instanceId: 't1-v2', tags: 'mine' });
    state = launchReducer(state, { type: LaunchActionType.Select, kind: LaunchTab.Workflow, refs: ['w'] });
    expect(state.items.map(launchItemKey)).toEqual(['tool:t1', 'pipeline:system/scan-all', 'workflow:w']);
  });
  it('removes one run and changes one run without touching the others', () => {
    let state = launchReducer(threeRuns(), { type: LaunchActionType.Remove, key: 'tool:t2' });
    expect(state.items.map(launchItemKey)).toEqual(['tool:t1', 'pipeline:system/scan-all']);
    state = launchReducer(state, { type: LaunchActionType.Item, key: 'pipeline:system/scan-all', patch: { sla: '60' } });
    expect(state.items[0].sla).toBe('');
    expect(state.items[1].sla).toBe('60');
  });
  it('keeps the shared settings and inputs apart from the runs', () => {
    let state = launchReducer(threeRuns(), { type: LaunchActionType.Shared, patch: { tags: 'case=1' } });
    state = launchReducer(state, {
      type: LaunchActionType.Inputs,
      inputs: { ...state.inputs, mode: InputsMode.Repos, repo: 'github.com/o/r' },
    });
    expect(state.shared).toEqual({ tags: 'case=1', sla: '' });
    expect(state.inputs.samples).toEqual([A]);
    expect(visibleInputs(state.inputs).samples).toEqual([]);
    expect(state.items).toHaveLength(3);
  });
  it('gives linked tool versions to their tools once, and the latest to the rest', () => {
    let state = initialLaunchState(new URLSearchParams('tool=t1&tool=t2&instance=t2-v1'));
    const tools = [
      { id: 't1', latest: 't1-v2', instances: ['t1-v1', 't1-v2'] },
      { id: 't2', latest: 't2-v2', instances: ['t2-v1', 't2-v2'] },
    ];
    state = launchReducer(state, { type: LaunchActionType.ToolVersions, tools });
    expect(state.items.map((item) => item.instanceId)).toEqual(['t1-v2', 't2-v1']);
    expect(state.linkedInstances).toBeNull();
    // a later read of the tool list leaves a version the user cleared alone
    state = launchReducer(state, { type: LaunchActionType.Item, key: 'tool:t1', patch: { instanceId: '' } });
    expect(launchReducer(state, { type: LaunchActionType.ToolVersions, tools })).toBe(state);
  });
  it('returns the state unchanged for an unknown change', () => {
    const state = threeRuns();
    expect(launchReducer(state, { type: 'bogus' } as unknown as LaunchAction)).toBe(state);
  });
  it('counts the runs of each kind for the tab badges', () => {
    expect(countByKind(threeRuns().items)).toEqual({ [LaunchTab.Workflow]: 0, [LaunchTab.Tool]: 2, [LaunchTab.Pipeline]: 1 });
  });
});

describe('describeItem', () => {
  const view = (patch: Partial<ReturnType<typeof newLaunchItem>> & { kind: LaunchTab; ref: string }, catalog = CATALOG) =>
    describeItem({ ...newLaunchItem(patch.kind, patch.ref), ...patch }, catalog, USER_GROUPS);
  it('lets a tool on a version in a single group run', () => {
    expect(view({ kind: LaunchTab.Tool, ref: 't1', instanceId: 't1-v2' })).toMatchObject({
      name: 'scan',
      groups: ['system'],
      blocker: null,
    });
  });
  it('names what a run lacks: a version, a group, valid args, a valid SLA, or a runnable tool', () => {
    expect(view({ kind: LaunchTab.Tool, ref: 't1' }).blocker).toBe('Choose a version of scan.');
    expect(view({ kind: LaunchTab.Tool, ref: 'm', instanceId: 'm-v2' }).blocker).toBe('Choose the group to run multi in.');
    expect(view({ kind: LaunchTab.Tool, ref: 'm', instanceId: 'm-v2', group: 'other' }).blocker).toBeNull();
    expect(
      view({ kind: LaunchTab.Tool, ref: 't1', instanceId: 't1-v2', args: { positionals: '', kwargs: '=x', switches: '' } }).blocker,
    ).toMatch(/KEY=VALUE/);
    expect(view({ kind: LaunchTab.Tool, ref: 't1', instanceId: 't1-v2', sla: 'soon' }).blocker).toBe('Fix the SLA of scan.');
    expect(view({ kind: LaunchTab.Tool, ref: 'h', instanceId: 'h-v2' }).blocker).toMatch(/harness/);
  });
  it('runs a workflow on its latest instance and refuses one with none', () => {
    expect(view({ kind: LaunchTab.Workflow, ref: 'w' })).toMatchObject({ instanceId: 'wi', blocker: null });
    const bare = { ...WORKFLOW, metadata: { WorkflowCatalog: { latest_instance: null } } } as unknown as Workflow;
    expect(view({ kind: LaunchTab.Workflow, ref: 'w' }, { ...CATALOG, workflows: [bare] }).blocker).toBe(
      'triage has no Ready instance yet.',
    );
  });
  it('marks a run found only once its entity is in the lists read so far', () => {
    expect(view({ kind: LaunchTab.Workflow, ref: 'w' }).found).toBe(true);
    expect(view({ kind: LaunchTab.Tool, ref: 't1' }).found).toBe(true);
    expect(view({ kind: LaunchTab.Pipeline, ref: 'system/scan-all' }).found).toBe(true);
    expect(view({ kind: LaunchTab.Workflow, ref: '../../users/delete-me' }).found).toBe(false);
    expect(view({ kind: LaunchTab.Workflow, ref: 'w' }, { ...CATALOG, workflows: [], loading: [LaunchTab.Workflow] }).found).toBe(false);
  });
  it('runs a pipeline in its group and refuses one with a legacy stage', () => {
    expect(view({ kind: LaunchTab.Pipeline, ref: 'system/scan-all' })).toMatchObject({ groups: ['system'], blocker: null });
    expect(view({ kind: LaunchTab.Pipeline, ref: 'system/legacy' })).toMatchObject({
      blocker: 'legacy cannot be run.',
    });
    expect(view({ kind: LaunchTab.Pipeline, ref: 'system/legacy' }).pipelineProblem).toContain('harvest');
  });
  it('waits for a list that is loading, and says when a linked run is not available', () => {
    expect(view({ kind: LaunchTab.Tool, ref: 'gone' }, { ...CATALOG, loading: [LaunchTab.Tool] }).blocker).toBe('Loading the tool list.');
    expect(view({ kind: LaunchTab.Tool, ref: 'gone' }).blocker).toBe('The tool gone is not available to you.');
  });
});

describe('launch blockers', () => {
  const views = threeRuns().items.map((item) => describeItem(item, CATALOG, USER_GROUPS));
  it('asks for a run first, then a valid shared SLA, then each run, then the inputs', () => {
    expect(launchBlocker([], BLANK_LAUNCH_STATE.shared, null)).toBe('Choose a workflow, tool, or pipeline to run.');
    expect(launchBlocker(views, { tags: '', sla: '-1' }, null)).toBe('Fix the SLA to run.');
    const unversioned = describeItem(newLaunchItem(LaunchTab.Tool, 't1'), CATALOG, USER_GROUPS);
    expect(launchBlocker([...views, unversioned], BLANK_LAUNCH_STATE.shared, 'inputs')).toBe('Choose a version of scan.');
    expect(launchBlocker(views, BLANK_LAUNCH_STATE.shared, 'Add a file.')).toBe('Add a file.');
    expect(launchBlocker(views, BLANK_LAUNCH_STATE.shared, null)).toBeNull();
  });
  it('requires an input when any run is a workflow or tool, and none for pipelines alone', () => {
    expect(launchRequirement(threeRuns().items)).toBe(InputsRequirement.AtLeastOne);
    expect(launchRequirement([newLaunchItem(LaunchTab.Pipeline, 'system/scan-all')])).toBe(InputsRequirement.Optional);
  });
  it('uploads to every group a run may use, each once', () => {
    const multi = describeItem({ ...newLaunchItem(LaunchTab.Tool, 'm', 'm-v2'), group: 'other' }, CATALOG, USER_GROUPS);
    expect(uploadGroups([...views, multi])).toEqual(['system', 'other']);
  });
  it('labels the button with the number of runs', () => {
    expect(launchLabel(1)).toBe('Launch 1 run');
    expect(launchLabel(3)).toBe('Launch 3 runs');
  });
});

describe('itemSubmission', () => {
  it('adds a run’s tags to the shared ones and lets its SLA replace the shared one', () => {
    const item = { ...newLaunchItem(LaunchTab.Tool, 't1', 't1-v2'), tags: 'mine', sla: '30' };
    expect(itemSettings(item, { tags: 'case=1', sla: '60' })).toEqual({ group: '', tags: 'case=1, mine', sla: '30' });
    expect(itemSettings(newLaunchItem(LaunchTab.Tool, 't1'), { tags: 'case=1', sla: '60' })).toEqual({
      group: '',
      tags: 'case=1',
      sla: '60',
    });
  });
  it('builds each kind’s request from its own options only', () => {
    const state = threeRuns();
    const views = state.items.map((item) => describeItem(item, CATALOG, USER_GROUPS));
    const inputs = { samples: [A], repos: [] };
    expect(itemSubmission(views[0], { tags: 'case=1', sla: '' }, inputs)).toMatchObject({
      tab: LaunchTab.Tool,
      instanceId: 't1-v2',
      request: { inputs: { samples: [A], repos: [] }, tags: ['case=1'] },
    });
    expect(itemSubmission(views[2], { tags: '', sla: '90' }, inputs)).toMatchObject({
      tab: LaunchTab.Pipeline,
      request: { group: 'system', pipeline: 'scan-all', samples: [A], sla: 90 },
    });
    const workflow = describeItem(newLaunchItem(LaunchTab.Workflow, 'w'), CATALOG, USER_GROUPS);
    expect(itemSubmission(workflow, BLANK_LAUNCH_STATE.shared, inputs)).toMatchObject({ tab: LaunchTab.Workflow, instanceId: 'wi' });
  });
  it('refuses a run that is blocked', () => {
    const blocked = describeItem(newLaunchItem(LaunchTab.Tool, 't1'), CATALOG, USER_GROUPS);
    expect(itemSubmission(blocked, BLANK_LAUNCH_STATE.shared, { samples: [], repos: [] })).toEqual({ error: 'Choose a version of scan.' });
  });
});

describe('launchRuns', () => {
  /** A sender that records every request and answers each with a new id, failing the runs named. */
  function recorder(failing: string[] = []) {
    const sent: LaunchSubmission[] = [];
    const send = (submission: LaunchSubmission, onError: (message: string) => void) => {
      sent.push(submission);
      if (failing.includes(submission.key)) {
        onError('Create run: no access trace: abc');
        return Promise.resolve(null);
      }
      return Promise.resolve({ id: `run-${sent.length}` });
    };
    return { sent, send };
  }
  /** An uploader that is never expected to be called. */
  const noUpload = () => Promise.reject(new Error('nothing should be uploaded'));
  it('sends one request per listed run, and nothing for a run that is not listed', async () => {
    // a third tool chosen and then removed must not be sent
    let state = launchReducer(threeRuns(), { type: LaunchActionType.Select, kind: LaunchTab.Tool, refs: ['t1', 't2', 'm'] });
    state = launchReducer(state, { type: LaunchActionType.Remove, key: 'tool:m' });
    const views = state.items.map((item) => describeItem(item, CATALOG, USER_GROUPS));
    const { sent, send } = recorder();
    const result = await launchRuns(views, state.shared, state.inputs, noUpload, send);
    expect(result.error).toBeNull();
    expect(sent.map((submission) => submission.key).sort()).toEqual(['pipeline:system/scan-all', 'tool:t1', 'tool:t2']);
    expect(sent.every((submission) => JSON.stringify(submission.request).includes(A))).toBe(true);
    expect(result.outcomes.map((outcome) => outcome.path)).toEqual([
      expect.stringMatching(/^\/runs\/system\/run-\d$/),
      expect.stringMatching(/^\/runs\/system\/run-\d$/),
      expect.stringMatching(/^\/runs\/system\/run-\d$/),
    ]);
  });
  it('reports a failing run beside the runs that started', async () => {
    const state = threeRuns();
    const views = state.items.map((item) => describeItem(item, CATALOG, USER_GROUPS));
    const { sent, send } = recorder(['tool:t2']);
    const result = await launchRuns(views, state.shared, state.inputs, noUpload, send);
    expect(sent).toHaveLength(3);
    expect(result.outcomes.map((outcome) => [outcome.key, outcome.error === '' ? 'ok' : outcome.error])).toEqual([
      ['tool:t1', 'ok'],
      ['tool:t2', 'Create run: no access trace: abc'],
      ['pipeline:system/scan-all', 'ok'],
    ]);
  });
  it('uploads the prompt once to every run group and gives every run the upload', async () => {
    const state = launchReducer(threeRuns(), { type: LaunchActionType.Inputs, inputs: { ...BLANK_INPUTS, prompt: 'look' } });
    const views = state.items.map((item) => describeItem(item, CATALOG, USER_GROUPS));
    const uploads: string[][] = [];
    const upload = (form: FormData) => {
      uploads.push(form.getAll('groups').map(String));
      return Promise.resolve(B);
    };
    const { sent, send } = recorder();
    await launchRuns(views, state.shared, state.inputs, upload, send);
    expect(uploads).toEqual([['system']]);
    expect(sent.every((submission) => JSON.stringify(submission.request).includes(B))).toBe(true);
  });
  it('starts nothing when an upload fails', async () => {
    const state = launchReducer(threeRuns(), { type: LaunchActionType.Inputs, inputs: { ...BLANK_INPUTS, prompt: 'look' } });
    const views = state.items.map((item) => describeItem(item, CATALOG, USER_GROUPS));
    const { sent, send } = recorder();
    const result = await launchRuns(views, state.shared, state.inputs, () => Promise.resolve(null), send);
    expect(result).toEqual({ error: 'The prompt could not be uploaded', outcomes: [] });
    expect(sent).toHaveLength(0);
  });
});

describe('mapWithConcurrency', () => {
  it('keeps at most the limit in flight and the results in order', async () => {
    let active = 0;
    let peak = 0;
    const results = await mapWithConcurrency([5, 1, 4, 2, 3], 2, async (delay) => {
      active += 1;
      peak = Math.max(peak, active);
      await new Promise((resolve) => setTimeout(resolve, delay));
      active -= 1;
      return delay * 10;
    });
    expect(results).toEqual([50, 10, 40, 20, 30]);
    expect(peak).toBe(2);
  });
});

describe('guardedLaunch', () => {
  it('starts one launch for two clicks in a row, and another once the first is done', async () => {
    const guard = { current: false };
    let runs = 0;
    const action = async () => {
      runs += 1;
      await new Promise((resolve) => setTimeout(resolve, 5));
      return runs;
    };
    const [first, second] = await Promise.all([guardedLaunch(guard, action), guardedLaunch(guard, action)]);
    expect([first, second]).toEqual([1, null]);
    expect(await guardedLaunch(guard, action)).toBe(2);
    expect(guard.current).toBe(false);
  });
  it('releases the guard when the launch throws', async () => {
    const guard = { current: false };
    await expect(guardedLaunch(guard, () => Promise.reject(new Error('boom')))).rejects.toThrow('boom');
    expect(guard.current).toBe(false);
  });
});

describe('launch URL', () => {
  it('builds a link with one sample parameter per sample', () => {
    expect(launchRunPath({ samples: [A] })).toBe(`/launch?sample=${A}`);
    expect(launchRunPath({ samples: [A, B] })).toBe(`/launch?sample=${A}&sample=${B}`);
    expect(launchRunPath({ repo: 'github.com/o/r:main' })).toBe('/launch?repo=github.com%2Fo%2Fr%3Amain');
    expect(launchRunPath({})).toBe('/launch');
  });
  it('reads linked samples into the Files mode, keeping a malformed one for its error token', () => {
    const state = initialLaunchState(new URLSearchParams(`sample=${A}&sample=nothex`));
    expect(state.inputs.mode).toBe(InputsMode.Files);
    expect(state.inputs.samples).toEqual([A, 'nothex']);
    expect(state.items).toEqual([]);
  });
  it('round-trips several runs of each kind, their tool versions, and the inputs, keeping the tab and other parameters', () => {
    let state = initialLaunchState(new URLSearchParams('workflow=w'));
    state = launchReducer(state, { type: LaunchActionType.Select, kind: LaunchTab.Tool, refs: ['t1', 't2'], latest: { t1: 't1-v2' } });
    state = launchReducer(state, { type: LaunchActionType.Item, key: 'tool:t2', patch: { instanceId: 't2-v1' } });
    state = launchReducer(state, { type: LaunchActionType.Select, kind: LaunchTab.Pipeline, refs: ['system/scan-all', 'other/x'] });
    state = launchReducer(state, {
      type: LaunchActionType.Inputs,
      inputs: { ...state.inputs, mode: InputsMode.Both, samples: [A, B], repo: 'github.com/o/r' },
    });
    const params = launchSearchParams(new URLSearchParams('tab=tool&other=1'), state);
    expect(params.get('tab')).toBe('tool');
    expect(params.get('other')).toBe('1');
    expect(params.getAll('tool')).toEqual(['t1', 't2']);
    expect(params.getAll('instance')).toEqual(['t1-v2', 't2-v1']);
    expect(params.getAll('pipeline')).toEqual(['system/scan-all', 'other/x']);
    expect(params.getAll('sample')).toEqual([A, B]);
    let reread = initialLaunchState(params);
    expect(reread.items.map(launchItemKey)).toEqual(['workflow:w', 'tool:t1', 'tool:t2', 'pipeline:system/scan-all', 'pipeline:other/x']);
    expect(reread.inputs).toMatchObject({ mode: InputsMode.Both, samples: [A, B], repo: 'github.com/o/r' });
    // the versions go back to their tools once the tools are known
    reread = launchReducer(reread, {
      type: LaunchActionType.ToolVersions,
      tools: [
        { id: 't1', latest: 't1-v2', instances: ['t1-v1', 't1-v2'] },
        { id: 't2', latest: 't2-v2', instances: ['t2-v1', 't2-v2'] },
      ],
    });
    expect(reread.items.filter((item) => item.kind === LaunchTab.Tool).map((item) => item.instanceId)).toEqual(['t1-v2', 't2-v1']);
    // until then the URL keeps the linked versions
    expect(launchSearchParams(new URLSearchParams(), initialLaunchState(params)).getAll('instance')).toEqual(['t1-v2', 't2-v1']);
  });
  it('reads a run named twice once', () => {
    expect(initialLaunchState(new URLSearchParams('tool=t1&tool=t1&tool=')).items.map(launchItemKey)).toEqual(['tool:t1']);
  });
  it('leaves typed values that are not shareable out of the URL', () => {
    let state = apply({ type: LaunchActionType.Inputs, inputs: { ...BLANK_INPUTS, prompt: 'secret prompt' } });
    state = launchReducer(state, { type: LaunchActionType.Shared, patch: { tags: 'secret-tag' } });
    expect(launchSearchParams(new URLSearchParams(), state).toString()).toBe('');
  });
  it('writes the mode only when the inputs alone would open another', () => {
    const state = apply({ type: LaunchActionType.Inputs, inputs: { ...BLANK_INPUTS, mode: InputsMode.Repos } });
    expect(launchSearchParams(new URLSearchParams(), state).get('inputs')).toBe('repos');
  });
});

describe('inputsSummary', () => {
  it('names the shown inputs every run receives', () => {
    expect(inputsSummary(BLANK_INPUTS)).toBe('Each run receives no inputs yet.');
    expect(inputsSummary({ ...BLANK_INPUTS, prompt: 'p', samples: [A] })).toBe('Each run receives the prompt and 1 existing sample.');
    expect(inputsSummary({ ...BLANK_INPUTS, mode: InputsMode.Both, samples: [A, B], repo: 'github.com/o/r' })).toBe(
      'Each run receives 2 existing samples and the repo github.com/o/r.',
    );
    // a hidden section is not named
    expect(inputsSummary({ ...BLANK_INPUTS, mode: InputsMode.Repos, samples: [A] })).toBe('Each run receives no inputs yet.');
  });
});
