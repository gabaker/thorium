// project imports
import { BLANK_INPUTS, initialInputs, InputsRequirement, InputsState, resolveInputs, SampleUploader } from './inputs';
import { buildJobArgs, buildToolRequest, buildWorkflowRequest, candidateGroups, LaunchTab, runPagePath, slaProblem } from './launch';
import { buildPipelineRunRequest, pipelineBlocker } from './pipelineRequest';
import { Pipeline } from '@models/pipelines';
import { ReactionRequest, RepoDependencyRequest } from '@models/reactions';
import { ToolRunRequest, WorkflowRunRequest } from '@models/runs';
import { Tool, ToolKind } from '@models/tools';
import { Workflow, WorkflowInstance } from '@models/workflows';

/** The optional settings a run takes: group, tags, and SLA, as typed. */
export interface RunSettingsValues {
  /** The chosen group, or an empty string to let the API pick */
  group: string;
  /** Tags as typed */
  tags: string;
  /** The SLA in seconds as typed */
  sla: string;
}

/** The job args of a direct tool run, as typed. */
export interface ToolArgsText {
  /** Positional args, one per line */
  positionals: string;
  /** Keyword args, one `KEY=VALUE` per line */
  kwargs: string;
  /** Switches, one per line or comma separated */
  switches: string;
}

/** One run the user chose to launch: a workflow, a tool, or a pipeline, with its own options. */
export interface LaunchItem {
  /** Which kind of run this is */
  kind: LaunchTab;
  /** The workflow's id, the tool's id, or the pipeline as `<group>/<name>` */
  ref: string;
  /** The chosen tool version (instance) id; empty for workflows and pipelines, or a tool with no version chosen */
  instanceId: string;
  /** The chosen run group, or an empty string to let the API pick (a pipeline always runs in its own group) */
  group: string;
  /** Tags added to the shared tags for this run only, as typed */
  tags: string;
  /** This run's SLA in seconds as typed; blank uses the shared SLA */
  sla: string;
  /** The job args of a tool run */
  args: ToolArgsText;
}

/** The tags and SLA every listed run gets unless it overrides them. */
export interface SharedSettings {
  /** Tags as typed, added to every run */
  tags: string;
  /** The SLA in seconds as typed; blank lets the API pick */
  sla: string;
}

/** Everything the launch page holds: the chosen runs across the tabs, their shared settings, and the shared inputs. */
export interface LaunchState {
  /** The runs to launch, in the order they were chosen */
  items: LaunchItem[];
  /** The tags and SLA every run gets */
  shared: SharedSettings;
  /** The prompt, files, samples, and repo every run receives */
  inputs: InputsState;
  /**
   * Tool versions named by the URL that wait for the tool list to say which tool each belongs to (empty when the URL
   * names tools but no versions); `null` once the linked tools have their versions, or when the URL names no tools.
   */
  linkedInstances: string[] | null;
}

/** Blank job args. */
export const BLANK_ARGS: ToolArgsText = { positionals: '', kwargs: '', switches: '' };

/** The blank shared settings. */
export const BLANK_SHARED: SharedSettings = { tags: '', sla: '' };

/** Nothing chosen and blank inputs, for tests and resets. */
export const BLANK_LAUNCH_STATE: LaunchState = { items: [], shared: BLANK_SHARED, inputs: BLANK_INPUTS, linkedInstances: null };

/**
 * Build a new run item with blank options.
 *
 * @param kind - The kind of run.
 * @param ref - The workflow id, tool id, or pipeline `<group>/<name>`.
 * @param instanceId - The tool version to start with, if any.
 * @returns The item.
 */
export function newLaunchItem(kind: LaunchTab, ref: string, instanceId = ''): LaunchItem {
  return { kind, ref, instanceId, group: '', tags: '', sla: '', args: BLANK_ARGS };
}

/**
 * The stable key of a run item; one item per workflow, tool, or pipeline.
 *
 * @param item - The item.
 * @returns `<kind>:<ref>`.
 */
export const launchItemKey = (item: Pick<LaunchItem, 'kind' | 'ref'>): string => `${item.kind}:${item.ref}`;

/** The URL parameter that names the runs of each kind, one parameter per run. */
const ITEM_PARAMS: Record<LaunchTab, string> = {
  [LaunchTab.Workflow]: 'workflow',
  [LaunchTab.Tool]: 'tool',
  [LaunchTab.Pipeline]: 'pipeline',
};

/**
 * Build the launch page's starting state from its URL: the linked workflows, tools, tool versions, pipelines, and
 * inputs.
 *
 * @param params - The launch page's URL parameters.
 * @returns The starting state.
 */
export function initialLaunchState(params: URLSearchParams): LaunchState {
  const items: LaunchItem[] = [];
  for (const kind of Object.values(LaunchTab)) {
    for (const ref of params.getAll(ITEM_PARAMS[kind])) {
      const item = newLaunchItem(kind, ref.trim());
      if (item.ref !== '' && !items.some((other) => launchItemKey(other) === launchItemKey(item))) items.push(item);
    }
  }
  // linked tools wait for the tool list to get their linked or latest versions
  const linked = items.some((item) => item.kind === LaunchTab.Tool) ? params.getAll('instance').filter((id) => id !== '') : null;
  return { items, shared: BLANK_SHARED, inputs: initialInputs(params), linkedInstances: linked };
}

/** The kinds of change to the launch page's state. */
export enum LaunchActionType {
  /** Set which runs of one kind are chosen */
  Select = 'select',
  /** Drop one run */
  Remove = 'remove',
  /** Change some options of one run */
  Item = 'item',
  /** Change the shared tags or SLA */
  Shared = 'shared',
  /** Replace the shared inputs */
  Inputs = 'inputs',
  /** Give the tools listed by the URL their linked or latest versions, once the tools are known */
  ToolVersions = 'tool-versions',
}

/** The parts of a tool that choosing its version reads. */
export type ToolVersions = { id: string; latest: string | null; instances: string[] };

/** A change to the launch page's state. */
export type LaunchAction =
  | { type: LaunchActionType.Select; kind: LaunchTab; refs: string[]; latest?: Record<string, string> }
  | { type: LaunchActionType.Remove; key: string }
  | { type: LaunchActionType.Item; key: string; patch: Partial<Omit<LaunchItem, 'kind' | 'ref'>> }
  | { type: LaunchActionType.Shared; patch: Partial<SharedSettings> }
  | { type: LaunchActionType.Inputs; inputs: InputsState }
  | { type: LaunchActionType.ToolVersions; tools: ToolVersions[] };

/**
 * Set which runs of one kind are chosen: chosen runs that stay keep their options, new ones are added at the end, and
 * the runs of other kinds are left as they were.
 *
 * @param items - The current runs.
 * @param kind - The kind whose choice changed.
 * @param refs - The chosen refs of that kind.
 * @param latest - The version to start each newly chosen tool on, by tool id.
 * @returns The new runs.
 */
function selectItems(items: LaunchItem[], kind: LaunchTab, refs: string[], latest: Record<string, string> = {}): LaunchItem[] {
  const kept = items.filter((item) => item.kind !== kind || refs.includes(item.ref));
  const added = refs
    .filter((ref) => !kept.some((item) => item.kind === kind && item.ref === ref))
    .map((ref) => newLaunchItem(kind, ref, latest[ref] ?? ''));
  return [...kept, ...added];
}

/**
 * Give each tool from the URL its linked version when one belongs to it, else its latest; a tool keeps a version it
 * already has.
 *
 * @param items - The current runs.
 * @param linked - The versions the URL named.
 * @param tools - The known tools.
 * @returns The runs with their tool versions filled in.
 */
function fillToolVersions(items: LaunchItem[], linked: string[], tools: ToolVersions[]): LaunchItem[] {
  return items.map((item) => {
    if (item.kind !== LaunchTab.Tool) return item;
    const tool = tools.find((entry) => entry.id === item.ref);
    if (!tool || tool.instances.includes(item.instanceId)) return item;
    const instanceId = linked.find((id) => tool.instances.includes(id)) ?? tool.latest ?? '';
    return { ...item, instanceId };
  });
}

/**
 * Apply a change to the launch page's state; a change to one run leaves the others as they were.
 *
 * @param state - The current state.
 * @param action - The change.
 * @returns The new state.
 */
export function launchReducer(state: LaunchState, action: LaunchAction): LaunchState {
  switch (action.type) {
    case LaunchActionType.Select:
      return { ...state, items: selectItems(state.items, action.kind, action.refs, action.latest) };
    case LaunchActionType.Remove:
      return { ...state, items: state.items.filter((item) => launchItemKey(item) !== action.key) };
    case LaunchActionType.Item:
      return { ...state, items: state.items.map((item) => (launchItemKey(item) === action.key ? { ...item, ...action.patch } : item)) };
    case LaunchActionType.Shared:
      return { ...state, shared: { ...state.shared, ...action.patch } };
    case LaunchActionType.Inputs:
      return { ...state, inputs: action.inputs };
    case LaunchActionType.ToolVersions:
      // the URL's versions are given out once; later tool list reads leave the chosen versions alone
      if (state.linkedInstances === null) return state;
      return { ...state, items: fillToolVersions(state.items, state.linkedInstances, action.tools), linkedInstances: null };
    default:
      return state;
  }
}

/**
 * Count the chosen runs of each kind, for the tab badges.
 *
 * @param items - The chosen runs.
 * @returns The number of runs of each kind.
 */
export function countByKind(items: LaunchItem[]): Record<LaunchTab, number> {
  const counts = { [LaunchTab.Workflow]: 0, [LaunchTab.Tool]: 0, [LaunchTab.Pipeline]: 0 };
  items.forEach((item) => {
    counts[item.kind] += 1;
  });
  return counts;
}

/** What the launch page knows about the workflows, tools, and pipelines the chosen runs name. */
export interface LaunchCatalog {
  /** The workflows the user can see */
  workflows: Workflow[];
  /** The tools the user can see */
  tools: Tool[];
  /** The pipelines of the user's groups */
  pipelines: Pipeline[];
  /** The workflow instances read so far, by id */
  instances: Record<string, WorkflowInstance | undefined>;
  /** The kinds whose list is still loading */
  loading: LaunchTab[];
}

/** A chosen run with what the page knows about it: its name, groups, the version it runs, and why it can't run yet. */
export interface LaunchItemView {
  /** The run's options */
  item: LaunchItem;
  /** The run's key */
  key: string;
  /** The workflow, tool, or pipeline name, or the ref while it is unknown */
  name: string;
  /** Whether the workflow, tool, or pipeline is in the lists read so far; `false` while loading or when it is not available */
  found: boolean;
  /** The groups the run may use: the entity's groups the user is in, or the pipeline's group */
  groups: string[];
  /** Every group of the entity */
  entityGroups: string[];
  /** The tool version or workflow instance the run starts; empty when there is none */
  instanceId: string;
  /** Why the run can't start, as a sentence; `null` when it can */
  blocker: string | null;
  /** The reason a pipeline can't run at all, shown with it; empty when it can */
  pipelineProblem: string;
}

/** The label of each kind of run, in the singular. */
export const LAUNCH_KIND_NOUNS: Record<LaunchTab, string> = {
  [LaunchTab.Workflow]: 'workflow',
  [LaunchTab.Tool]: 'tool',
  [LaunchTab.Pipeline]: 'pipeline',
};

/**
 * Split a pipeline key into its group and name.
 *
 * @param key - The pipeline as `<group>/<name>`.
 * @returns The group and name, or `null` when the key has no group.
 */
export function splitPipelineKey(key: string): { group: string; name: string } | null {
  // a group name has no slash, so the first one splits the key
  const at = key.indexOf('/');
  if (at < 1) return null;
  return { group: key.slice(0, at), name: key.slice(at + 1) };
}

/**
 * Work out what the page knows about one chosen run and whether it can start.
 *
 * @param item - The chosen run.
 * @param catalog - The known workflows, tools, pipelines, and workflow instances.
 * @param userGroups - The signed in user's groups.
 * @returns The run's view.
 */
export function describeItem(item: LaunchItem, catalog: LaunchCatalog, userGroups: string[]): LaunchItemView {
  const key = launchItemKey(item);
  const base = {
    item,
    key,
    name: item.ref,
    found: false,
    groups: [] as string[],
    entityGroups: [] as string[],
    instanceId: '',
    pipelineProblem: '',
  };
  const loading = catalog.loading.includes(item.kind);
  const noun = LAUNCH_KIND_NOUNS[item.kind];
  const missing = loading ? `Loading the ${noun} list.` : `The ${noun} ${item.ref} is not available to you.`;
  // the run group is required when more than one is possible
  const groupBlocker = (name: string, groups: string[]) =>
    item.group === '' && groups.length > 1 ? `Choose the group to run ${name} in.` : null;
  const slaBlocker = (name: string) => (slaProblem(item.sla) !== '' ? `Fix the SLA of ${name}.` : null);
  if (item.kind === LaunchTab.Pipeline) {
    const parts = splitPipelineKey(item.ref);
    const pipeline = catalog.pipelines.find((entry) => `${entry.group}/${entry.name}` === item.ref);
    if (!parts || !pipeline) return { ...base, name: parts?.name ?? item.ref, blocker: missing };
    const problem = pipelineBlocker(pipeline) ?? '';
    const blocker = problem !== '' ? `${pipeline.name} cannot be run.` : slaBlocker(pipeline.name);
    return {
      ...base,
      found: true,
      name: pipeline.name,
      groups: [pipeline.group],
      entityGroups: [pipeline.group],
      pipelineProblem: problem,
      blocker,
    };
  }
  if (item.kind === LaunchTab.Workflow) {
    const workflow = catalog.workflows.find((entry) => entry.id === item.ref);
    if (!workflow) return { ...base, blocker: missing };
    const instanceId = workflow.metadata.WorkflowCatalog.latest_instance ?? '';
    const entityGroups = catalog.instances[instanceId]?.groups ?? workflow.groups;
    const groups = candidateGroups(entityGroups, userGroups);
    const blocker =
      instanceId === ''
        ? `${workflow.name} has no Ready instance yet.`
        : (groupBlocker(workflow.name, groups) ?? slaBlocker(workflow.name));
    return { ...base, found: true, name: workflow.name, groups, entityGroups, instanceId, blocker };
  }
  const tool = catalog.tools.find((entry) => entry.id === item.ref);
  if (!tool) return { ...base, blocker: missing };
  const groups = candidateGroups(tool.groups, userGroups);
  const view = { ...base, found: true, name: tool.name, groups, entityGroups: tool.groups, instanceId: item.instanceId };
  // Harness tools can't be run directly
  if (tool.metadata.Tool.kind === ToolKind.Harness) return { ...view, blocker: `${tool.name} is a harness and cannot be run directly.` };
  if (item.instanceId === '') return { ...view, blocker: `Choose a version of ${tool.name}.` };
  const args = buildJobArgs(item.args.positionals, item.args.kwargs, item.args.switches);
  const argsBlocker = args.error ? `${tool.name}: ${args.error}.` : null;
  return { ...view, blocker: groupBlocker(tool.name, groups) ?? argsBlocker ?? slaBlocker(tool.name) };
}

/**
 * What the shared inputs must hold for the chosen runs: workflow and tool runs need at least one input, a pipeline
 * may run with none.
 *
 * @param items - The chosen runs.
 * @returns The strictest requirement of the chosen runs.
 */
export function launchRequirement(items: Pick<LaunchItem, 'kind'>[]): InputsRequirement {
  return items.length === 0 || items.some((item) => item.kind !== LaunchTab.Pipeline)
    ? InputsRequirement.AtLeastOne
    : InputsRequirement.Optional;
}

/**
 * Name why the chosen runs can't be launched yet, for the hint beside the disabled launch button.
 *
 * @param views - The chosen runs.
 * @param shared - The shared tags and SLA.
 * @param inputsBlocker - Why the shared inputs aren't ready, or `null` when they are.
 * @returns A sentence naming the first cause, or `null` when every run can start.
 */
export function launchBlocker(views: LaunchItemView[], shared: SharedSettings, inputsBlocker: string | null): string | null {
  if (views.length === 0) return 'Choose a workflow, tool, or pipeline to run.';
  if (slaProblem(shared.sla) !== '') return 'Fix the SLA to run.';
  return views.find((view) => view.blocker !== null)?.blocker ?? inputsBlocker;
}

/**
 * The groups to upload new samples to: every group a chosen run may use, so each run can read the uploads.
 *
 * @param views - The chosen runs.
 * @returns The groups, each once, in run order.
 */
export function uploadGroups(views: LaunchItemView[]): string[] {
  const groups = views.flatMap((view) => (view.item.group !== '' ? [view.item.group] : view.groups));
  return [...new Set(groups)];
}

/** The resolved inputs of a run: the samples (uploads included) and repos to send. */
export interface ResolvedInputs {
  /** The sample SHA256s */
  samples: string[];
  /** The repos */
  repos: RepoDependencyRequest[];
}

/** One request a launch sends: the run it is for, which endpoint it goes to, and its body. */
export type LaunchSubmission =
  | { key: string; tab: LaunchTab.Workflow; instanceId: string; request: WorkflowRunRequest }
  | { key: string; tab: LaunchTab.Tool; instanceId: string; request: ToolRunRequest }
  | { key: string; tab: LaunchTab.Pipeline; request: ReactionRequest };

/** Why a launch request couldn't be built. */
export interface SubmissionError {
  /** The reason, as a sentence */
  error: string;
}

/**
 * Build the request of a workflow run.
 *
 * @param key - The run's key.
 * @param instanceId - The workflow instance to run.
 * @param settings - The run's group, tags, and SLA.
 * @param inputs - The resolved shared inputs.
 * @returns The request for the workflow instance's runs route.
 */
export function workflowSubmission(
  key: string,
  instanceId: string,
  settings: RunSettingsValues,
  inputs: ResolvedInputs,
): Extract<LaunchSubmission, { tab: LaunchTab.Workflow }> {
  return { key, tab: LaunchTab.Workflow, instanceId, request: buildWorkflowRequest({ ...settings, ...inputs }) };
}

/**
 * Build the request of a direct tool run.
 *
 * @param key - The run's key.
 * @param instanceId - The tool version to run.
 * @param settings - The run's group, tags, and SLA.
 * @param args - The job args as typed.
 * @param inputs - The resolved shared inputs.
 * @returns The request and the tool instance it runs, or why the typed args are malformed.
 */
export function toolSubmission(
  key: string,
  instanceId: string,
  settings: RunSettingsValues,
  args: ToolArgsText,
  inputs: ResolvedInputs,
): Extract<LaunchSubmission, { tab: LaunchTab.Tool }> | SubmissionError {
  const built = buildJobArgs(args.positionals, args.kwargs, args.switches);
  if (!built.args) return { error: built.error ?? 'The args are invalid' };
  return { key, tab: LaunchTab.Tool, instanceId, request: buildToolRequest({ ...settings, ...inputs }, built.args) };
}

/**
 * Build the request of a pipeline run.
 *
 * @param key - The run's key.
 * @param pipelineKey - The pipeline as `<group>/<name>`.
 * @param settings - The run's tags and SLA; a pipeline always runs in its own group.
 * @param inputs - The resolved shared inputs.
 * @returns The reaction request, or a reason when the key names no pipeline.
 */
export function pipelineSubmission(
  key: string,
  pipelineKey: string,
  settings: Omit<RunSettingsValues, 'group'>,
  inputs: ResolvedInputs,
): Extract<LaunchSubmission, { tab: LaunchTab.Pipeline }> | SubmissionError {
  const parts = splitPipelineKey(pipelineKey);
  if (!parts) return { error: 'Choose a pipeline' };
  return {
    key,
    tab: LaunchTab.Pipeline,
    request: buildPipelineRunRequest({ ...settings, ...inputs, group: parts.group, pipeline: parts.name }),
  };
}

/**
 * Merge the shared tags and SLA with a run's own: the run's tags are added to the shared ones, and its SLA, when
 * given, replaces the shared one.
 *
 * @param item - The run.
 * @param shared - The shared settings.
 * @returns The run's group, tags, and SLA.
 */
export function itemSettings(item: LaunchItem, shared: SharedSettings): RunSettingsValues {
  const tags = [shared.tags, item.tags].filter((entry) => entry.trim() !== '').join(', ');
  return { group: item.group, tags, sla: item.sla.trim() !== '' ? item.sla : shared.sla };
}

/**
 * Build the request of one chosen run; nothing but the run's own options, the shared settings, and the inputs takes
 * part.
 *
 * @param view - The chosen run.
 * @param shared - The shared tags and SLA.
 * @param inputs - The resolved shared inputs.
 * @returns The request, or the reason it can't be built.
 */
export function itemSubmission(view: LaunchItemView, shared: SharedSettings, inputs: ResolvedInputs): LaunchSubmission | SubmissionError {
  const { item, key } = view;
  if (view.blocker !== null) return { error: view.blocker };
  const settings = itemSettings(item, shared);
  if (item.kind === LaunchTab.Workflow) return workflowSubmission(key, view.instanceId, settings, inputs);
  if (item.kind === LaunchTab.Tool) return toolSubmission(key, view.instanceId, settings, item.args, inputs);
  return pipelineSubmission(key, item.ref, { tags: settings.tags, sla: settings.sla }, inputs);
}

/**
 * Find the page of a run just launched.
 *
 * @param view - The chosen run.
 * @param id - The new run's id.
 * @returns The run's details page, or the runs list when its group is unknown.
 */
export function launchedRunPath(view: LaunchItemView, id: string): string {
  return runPagePath(id, view.item.group, view.groups, view.entityGroups);
}

/** What happened to one run of a launch. */
export interface LaunchOutcome {
  /** The run's key */
  key: string;
  /** The run's kind */
  kind: LaunchTab;
  /** The workflow, tool, or pipeline name */
  name: string;
  /** The page of the run started; empty when it failed */
  path: string;
  /** Why the run didn't start; empty when it started */
  error: string;
}

/** How many runs a launch starts at once. */
export const LAUNCH_CONCURRENCY = 3;

/**
 * Run an async step over a list with at most `limit` steps in flight, keeping the results in list order.
 *
 * @template T - The type of the list entries.
 * @template R - The type of a step's result.
 * @param entries - The entries to process.
 * @param limit - The most steps in flight at once (at least one).
 * @param step - The step; a step that throws rejects the whole run, so steps report failures in their result.
 * @returns The results, in the order of the entries.
 */
export async function mapWithConcurrency<T, R>(entries: T[], limit: number, step: (entry: T) => Promise<R>): Promise<R[]> {
  const results: R[] = new Array<R>(entries.length);
  let next = 0;
  // each worker takes the next unstarted entry until none are left
  const worker = async () => {
    while (next < entries.length) {
      const index = next;
      next += 1;
      results[index] = await step(entries[index]);
    }
  };
  await Promise.all(Array.from({ length: Math.max(1, Math.min(limit, entries.length)) }, worker));
  return results;
}

/**
 * Run an action unless one started through the same guard is still running, so a second click can't start a second
 * launch.
 *
 * @template R - The type of the action's result.
 * @param guard - Set for the whole of an action; a ref kept by the caller.
 * @param action - The action.
 * @returns The action's result, or `null` when another action was still running.
 */
export async function guardedLaunch<R>(guard: { current: boolean }, action: () => Promise<R>): Promise<R | null> {
  if (guard.current) return null;
  guard.current = true;
  try {
    return await action();
  } finally {
    guard.current = false;
  }
}

/** Sends one run request and returns the new run's id, or `null` after reporting why it failed. */
export type RunSender = (submission: LaunchSubmission, onError: (message: string) => void) => Promise<{ id: string } | null>;

/** The result of a launch: why nothing was started, or what happened to each run. */
export interface LaunchResult {
  /** Why no run was started (an input that couldn't be uploaded or is missing); `null` when the runs were sent */
  error: string | null;
  /** What happened to each run, in the order of the runs */
  outcomes: LaunchOutcome[];
}

/**
 * Launch every chosen run: upload the shared inputs once to every group the runs may use, then send one request per
 * run with at most `limit` in flight. A run that fails is reported beside the others and doesn't stop them.
 *
 * @param views - The chosen runs; nothing else is sent.
 * @param shared - The shared tags and SLA.
 * @param inputs - The shared inputs, hidden sections included; only the shown sections are sent.
 * @param upload - Uploads a sample form and returns its SHA256.
 * @param send - Sends one run request.
 * @param limit - The most requests in flight at once.
 * @returns Why nothing was started, or each run's outcome.
 */
export async function launchRuns(
  views: LaunchItemView[],
  shared: SharedSettings,
  inputs: InputsState,
  upload: SampleUploader,
  send: RunSender,
  limit = LAUNCH_CONCURRENCY,
): Promise<LaunchResult> {
  const resolved = await resolveInputs(inputs, uploadGroups(views), upload, launchRequirement(views.map((view) => view.item)));
  if (resolved.error) return { error: resolved.error, outcomes: [] };
  const outcomes = await mapWithConcurrency(views, limit, async (view): Promise<LaunchOutcome> => {
    const outcome = { key: view.key, kind: view.item.kind, name: view.name, path: '', error: '' };
    const submission = itemSubmission(view, shared, resolved);
    if ('error' in submission) return { ...outcome, error: submission.error };
    let message = '';
    const created = await send(submission, (reason) => {
      message = reason;
    });
    if (!created) return { ...outcome, error: message || 'The run was not started' };
    return { ...outcome, path: launchedRunPath(view, created.id) };
  });
  return { error: null, outcomes };
}

/** The URL parameters the launch page keeps in step with its state; the rest of the form stays out of the URL. */
const SYNCED_PARAMS = ['workflow', 'tool', 'instance', 'pipeline', 'inputs', 'sample', 'repo', 'repo_kind'];

/**
 * Write the shareable part of the launch page's state into its URL parameters: the chosen workflows, tools, tool
 * versions, and pipelines (one parameter each), the inputs mode, the samples (one `sample` parameter each), and the
 * repo.
 *
 * @param current - The page's current URL parameters; the tab and any parameter this page doesn't own are kept.
 * @param state - The launch page's state.
 * @returns The new URL parameters, which {@link initialLaunchState} reads back to the same choices.
 */
export function launchSearchParams(current: URLSearchParams, state: LaunchState): URLSearchParams {
  const params = new URLSearchParams(current);
  SYNCED_PARAMS.forEach((name) => params.delete(name));
  const { items, inputs } = state;
  for (const kind of Object.values(LaunchTab)) {
    items.filter((item) => item.kind === kind).forEach((item) => params.append(ITEM_PARAMS[kind], item.ref));
  }
  // a tool version names its tool, so the versions need no pairing with the tools
  const versions = items.filter((item) => item.kind === LaunchTab.Tool && item.instanceId !== '').map((item) => item.instanceId);
  [...new Set([...versions, ...(state.linkedInstances ?? [])])].forEach((id) => params.append('instance', id));
  // the mode is written only when it isn't the one the inputs alone would open with
  if (initialInputs(launchInputParams(inputs)).mode !== inputs.mode) params.set('inputs', inputs.mode);
  inputs.samples.forEach((sample) => params.append('sample', sample));
  if (inputs.repo.trim() !== '') params.set('repo', inputs.repo.trim());
  if (inputs.repo.trim() !== '' && inputs.repoKind !== BLANK_INPUTS.repoKind) params.set('repo_kind', inputs.repoKind);
  return params;
}

/**
 * The URL parameters that name a set of inputs, without a mode.
 *
 * @param inputs - The inputs.
 * @returns One `sample` parameter per sample and the `repo` parameter when a repo is given.
 */
function launchInputParams(inputs: Pick<InputsState, 'samples' | 'repo'>): URLSearchParams {
  const params = new URLSearchParams();
  inputs.samples.forEach((sample) => params.append('sample', sample));
  if (inputs.repo.trim() !== '') params.set('repo', inputs.repo.trim());
  return params;
}

/**
 * Build a link to the launch page with inputs filled in, for a Run button on a sample, a repo, or a selection of
 * samples.
 *
 * @param inputs - The samples (SHA256s) and the repo (`<url>[:<commitish>]`) to fill in; either may be left out.
 * @returns The launch page path; the samples open the Files mode, a repo alone opens the Repos mode.
 */
export function launchRunPath(inputs: { samples?: string[]; repo?: string }): string {
  const query = launchInputParams({ samples: inputs.samples ?? [], repo: inputs.repo ?? '' }).toString();
  return query === '' ? '/launch' : `/launch?${query}`;
}

/** The label of each launch tab. */
export const LAUNCH_TAB_LABELS: Record<LaunchTab, string> = {
  [LaunchTab.Workflow]: 'Workflows',
  [LaunchTab.Tool]: 'Tools',
  [LaunchTab.Pipeline]: 'Pipelines',
};

/**
 * Label the launch button with the number of runs it starts.
 *
 * @param count - The number of chosen runs.
 * @returns "Launch 1 run" or "Launch N runs".
 */
export const launchLabel = (count: number): string => `Launch ${count} run${count === 1 ? '' : 's'}`;
