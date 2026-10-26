// project imports
import { CommitishKinds, Reaction, ReactionStatus, RepoDependencyRequest, StatusUpdate } from '@models/reactions';
import { ReactionSource, RunKind, RunProvenance } from '@models/runs';

/**
 * Tell whether a run has ended.
 *
 * @param status - The run's status.
 * @returns `true` for Completed and Failed.
 */
export function isRunFinished(status: ReactionStatus): boolean {
  return status === ReactionStatus.Completed || status === ReactionStatus.Failed;
}

/** What the legacy reaction page shows for the outcome of its latest reads. */
export enum ReactionReadView {
  /** The reaction was read (or the first read is still pending), so its details show */
  Details = 'Details',
  /** The reaction was read before but the latest poll failed, so its details show with a warning */
  StaleDetails = 'StaleDetails',
  /** The reaction was never read, so only a not-found message shows */
  NotFound = 'NotFound',
}

/**
 * Choose what the legacy reaction page shows given its latest reads.
 *
 * A failed poll of a reaction that already loaded must not replace it with a not-found message,
 * because the reaction is known to exist.
 *
 * @param loaded - Whether any read of the reaction has succeeded.
 * @param error - The error of the latest read, empty when it succeeded.
 * @returns Which view to render.
 */
export function reactionReadView(loaded: boolean, error: string): ReactionReadView {
  if (!error) return ReactionReadView.Details;
  return loaded ? ReactionReadView.StaleDetails : ReactionReadView.NotFound;
}

/**
 * Explain why a finished run has no result on an input.
 *
 * A completed run of a tool normally stores a result, but Thorium keeps only the newest results under each key, so an
 * older run's result may have been pruned since; a failed run never stored one.
 *
 * @param status - The run's status.
 * @returns The sentence to show in place of the run's results.
 */
export function noResultsMessage(status: ReactionStatus): string {
  // a completed run's missing result was pruned, not never stored
  if (status === ReactionStatus.Completed) {
    return "No result from this run is kept on this input. Thorium keeps only the newest results under each key, so an older run's result may have been pruned.";
  }
  return 'This run stored no results on this input.';
}

/**
 * Get the kind of run a reaction is from its source.
 *
 * @param reaction - The reaction.
 * @returns The run kind; a reaction without a source (an older API) is a pipeline reaction.
 */
export function runKind(reaction: Pick<Reaction, 'source'>): RunKind {
  const source: ReactionSource | undefined = reaction.source;
  if (source && 'WorkflowRun' in source) return RunKind.Workflow;
  if (source && 'Tool' in source) return RunKind.Tool;
  return RunKind.Pipeline;
}

/**
 * Get the exact versions a reaction was created with.
 *
 * @param reaction - The reaction.
 * @returns The provenance, or `null` when the reaction recorded none.
 */
export function runProvenance(reaction: Pick<Reaction, 'source'>): RunProvenance | null {
  const source = reaction.source;
  if (!source) return null;
  if ('WorkflowRun' in source) return source.WorkflowRun.provenance;
  if ('Tool' in source) return source.Tool.provenance;
  return source.Pipeline.provenance;
}

/**
 * Get the key a run's results are stored under on each input.
 *
 * A workflow run records its key, a tool run stores results under its stage's tool, and a
 * pipeline reaction has no single key.
 *
 * @param reaction - The reaction.
 * @returns The result key, or `null` for a pipeline reaction.
 */
export function runResultKey(reaction: Pick<Reaction, 'source' | 'plan'>): string | null {
  const source = reaction.source;
  if (source && 'WorkflowRun' in source) return source.WorkflowRun.result_key;
  if (source && 'Tool' in source) return reaction.plan?.stages[0]?.[0]?.name ?? null;
  return null;
}

/** The span of time a run was alive, as epoch milliseconds. */
export interface RunWindow {
  /** When the run was created, or `null` when its updates don't say */
  start: number | null;
  /** When the run completed or failed, or `null` while it works or when its updates don't say */
  end: number | null;
}

/** How far outside a run's lifetime a result's upload time may fall and still count as the run's, in milliseconds. */
const RESULT_WINDOW_SLACK_MS = 1000;

/**
 * Find when a run started and ended from its status updates.
 *
 * @param updates - The run's status updates.
 * @returns The creation and end times; a time the updates don't record is `null`.
 */
export function runWindow(updates: Pick<StatusUpdate, 'action' | 'timestamp'>[]): RunWindow {
  const instant = (action: string[]) => {
    // a job reset arrives as an object, never one of the reaction actions looked for here
    const found = updates.find((update) => typeof update.action === 'string' && action.includes(update.action));
    const time = found ? Date.parse(found.timestamp) : NaN;
    return Number.isNaN(time) ? null : time;
  };
  return { start: instant(['ReactionCreated']), end: instant(['ReactionCompleted', 'ReactionFailed']) };
}

/**
 * Tell whether a run can be trusted to have stored the results found in its lifetime.
 *
 * Results don't record which run stored them, so a result stored while a run was alive may come
 * from another run of the same tool on the same input. A run whose job never reported completing
 * (a cancelled run, for one) most likely stored nothing, and neither did most failed runs.
 *
 * @param status - The run's status.
 * @param updates - The run's status updates.
 * @returns `true` when the run did not fail and one of its jobs completed.
 */
export function runResultsConfirmed(status: ReactionStatus, updates: Pick<StatusUpdate, 'action'>[]): boolean {
  return status !== ReactionStatus.Failed && updates.some((update) => update.action === 'JobCompleted');
}

/**
 * Keep the results that were stored while a run was alive.
 *
 * A result key is shared by every run of a tool (or workflow instance) on an input, so the
 * results stored outside a run's lifetime belong to other runs.
 *
 * @param outputs - The results stored under one key, in any order.
 * @param window - The run's lifetime.
 * @returns The results uploaded between the run's creation and end, with a second of slack on each side. A result
 *   with no readable upload time is kept, and so is every result when the lifetime is unknown.
 */
export function resultsDuringRun<T extends { uploaded?: string }>(outputs: T[], window: RunWindow): T[] {
  return outputs.filter((output) => {
    const uploaded = output.uploaded ? Date.parse(output.uploaded) : NaN;
    if (Number.isNaN(uploaded)) return true;
    if (window.start !== null && uploaded < window.start - RESULT_WINDOW_SLACK_MS) return false;
    return window.end === null || uploaded <= window.end + RESULT_WINDOW_SLACK_MS;
  });
}

/**
 * List the tags a user put on a run.
 *
 * The API also tags every reaction with its creator and its input files, and a workflow run with
 * `WorkflowRun`, its workflow, and its instance. The run's page shows those facts elsewhere.
 *
 * @param run - The run.
 * @returns The tags that are none of the automatic ones, in order.
 */
export function userRunTags(run: Pick<Reaction, 'tags' | 'creator' | 'samples' | 'repos' | 'source'>): string[] {
  const workflow = run.source && 'WorkflowRun' in run.source;
  return run.tags.filter((tag) => {
    if (tag === run.creator || run.samples.includes(tag) || run.repos.some((repo) => repo.url === tag)) return false;
    return !(workflow && (tag === 'WorkflowRun' || tag.startsWith('Workflow=') || tag.startsWith('WorkflowInstance=')));
  });
}

/**
 * Whether a stage is known not to belong to a reaction, so it can never have logs.
 *
 * @param stage - The stage whose logs were asked for.
 * @param stages - The stage names the reaction has, or `null` when they could not be read.
 * @returns `true` when the stage names are known and none of them is `stage`.
 */
export function isUnknownStage(stage: string, stages: string[] | null): boolean {
  return !!stages && stages.length > 0 && !stages.includes(stage);
}

/**
 * Say what an empty stage log page means for the reaction it belongs to.
 *
 * A stage that is not one of the reaction's stages will never have logs, and a finished reaction writes no more, so only
 * a stage that may still start or run is described as not logged "yet".
 *
 * @param stage - The stage whose logs were asked for.
 * @param status - The reaction's status, or `null` when it could not be read.
 * @param stages - The stage names the reaction has, or `null` when they could not be read.
 * @returns The sentence to show in place of the missing logs.
 */
export function noStageLogsMessage(stage: string, status: ReactionStatus | null, stages: string[] | null): string {
  // a stage the reaction does not have can never log, so say so and name the ones it does have
  if (stages && isUnknownStage(stage, stages)) {
    return `This reaction has no stage named ${stage}. Its stages are: ${stages.join(', ')}.`;
  }
  // a finished reaction writes no more logs, so the absence is final
  if (status && isRunFinished(status)) {
    return `No logs were recorded for the ${stage} stage of this reaction`;
  }
  return `No logs have been recorded for the ${stage} stage of this reaction yet`;
}

/**
 * List the stage names of a run's plan in order.
 *
 * @param reaction - The reaction.
 * @returns Every planned stage name, parallel stages in their listed order.
 */
export function plannedStageNames(reaction: Pick<Reaction, 'plan'>): string[] {
  return (reaction.plan?.stages ?? []).flat().map((stage) => stage.name);
}

/**
 * Split text into trimmed non-empty entries on newlines and commas, skipping `#` comments.
 *
 * Entries that start with `#` are comments (as in annotated hash lists); use
 * {@link splitPlainEntries} for values that may start with `#`, like tags.
 *
 * @param text - The text to split.
 * @returns The entries in order, without comments.
 */
export function splitEntries(text: string): string[] {
  return splitPlainEntries(text).filter((entry) => !entry.startsWith('#'));
}

/**
 * Split text into trimmed non-empty entries on newlines and commas, keeping every entry.
 *
 * @param text - The text to split.
 * @returns The entries in order.
 */
export function splitPlainEntries(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((entry) => entry.trim())
    .filter((entry) => entry !== '');
}

/**
 * Read a repo input written as `<url>[:<commitish>]`.
 *
 * A colon that belongs to a scheme (`https://`) or a port is not a commitish separator: only a
 * colon after the last `/` splits the commitish off.
 *
 * @param raw - The repo text.
 * @param kind - The kind of commitish to attach when one is given.
 * @returns The repo request, or `null` for blank text.
 */
export function parseRepoInput(raw: string, kind: CommitishKinds = CommitishKinds.Branch): RepoDependencyRequest | null {
  const text = raw.trim();
  if (text === '') return null;
  const slash = text.lastIndexOf('/');
  const colon = text.indexOf(':', slash + 1);
  // only a colon after the last slash separates a commitish
  if (colon > -1) {
    const commitish = text.slice(colon + 1);
    return commitish === '' ? { url: text.slice(0, colon) } : { url: text.slice(0, colon), commitish, kind };
  }
  return { url: text };
}

/**
 * Name the inputs problem of a launch form, if it has one.
 *
 * @param samples - The sample SHA256s chosen.
 * @param repos - The repos chosen.
 * @param hasPrompt - Whether prompt text was typed.
 * @returns A message when the run would have no input, otherwise `null`.
 */
export function inputsProblem(samples: string[], repos: RepoDependencyRequest[], hasPrompt: boolean): string | null {
  if (samples.length === 0 && repos.length === 0 && !hasPrompt) {
    return 'A run needs at least one input: type a prompt, choose a file, or choose a repo';
  }
  return null;
}

/**
 * Check typed sample hashes before they are sent with a run.
 *
 * @param samples - The typed entries.
 * @returns A message naming the first entry that isn't a SHA256 (64 hex characters), or `null` when all are.
 */
export function sampleHashProblem(samples: string[]): string | null {
  const bad = samples.find((sample) => !/^[0-9a-fA-F]{64}$/.test(sample));
  return bad === undefined ? null : `"${bad}" is not a sample SHA256: each existing sample must be 64 hex characters`;
}

/**
 * Turn a typed prompt into a file the run reads as a sample.
 *
 * A run's inputs are files and repos, and an agent's task prompt is fixed on the agent, so a
 * per-run prompt travels as a small text file input.
 *
 * @param prompt - The prompt text.
 * @param name - The file name to give it.
 * @returns A text file holding the prompt.
 */
export function promptFile(prompt: string, name = 'prompt.txt'): File {
  return new File([prompt], name, { type: 'text/plain' });
}

/**
 * Read a workflow run's result key into its workflow name, revision, and instance id.
 *
 * @param key - A result key such as `triage#r3~<instance id>`.
 * @returns The parts, or `null` when the key is not a workflow result key.
 */
export function parseWorkflowResultKey(key: string): { name: string; revision: number; instance: string } | null {
  const match = /^(.+)#r(\d+)~(.+)$/.exec(key);
  return match ? { name: match[1], revision: Number.parseInt(match[2], 10), instance: match[3] } : null;
}

/**
 * Get the name to show for a result key.
 *
 * A workflow run's result key is `{name}#r{revision}~{instance id}`; its Workflow Instance is
 * named `{name}#r{revision}`, so the id suffix is dropped. Any other key (a tool name) is shown as is.
 *
 * @param key - The key a result is stored under.
 * @returns The Workflow Instance name for a workflow result key, otherwise the key.
 */
export function resultDisplayName(key: string): string {
  const parsed = parseWorkflowResultKey(key);
  return parsed ? `${parsed.name}#r${parsed.revision}` : key;
}

/**
 * Get the page that shows a reaction.
 *
 * Tool and workflow runs open in the run view, which holds their plan, provenance, and results;
 * a pipeline reaction opens in the reaction page.
 *
 * @param reaction - The reaction.
 * @returns The route of the reaction's page.
 */
export function reactionPath(reaction: Pick<Reaction, 'source' | 'group' | 'id'>): string {
  const base = runKind(reaction) === RunKind.Pipeline ? 'reaction' : 'runs';
  return `/${base}/${reaction.group}/${reaction.id}`;
}

/**
 * Build the multipart form that uploads a file as a sample for a run to read.
 *
 * @param file - The file to upload.
 * @param groups - The groups to upload the file to.
 * @param description - A description for the submission.
 * @returns The form for `POST /files/`.
 */
export function buildSampleForm(file: File, groups: string[], description = ''): FormData {
  const form = new FormData();
  form.append('data', file);
  // the upload route reads each group from a repeated plain `groups` field
  for (const group of groups) {
    form.append('groups', group);
  }
  if (description.trim() !== '') {
    form.append('description', description.trim());
  }
  return form;
}
