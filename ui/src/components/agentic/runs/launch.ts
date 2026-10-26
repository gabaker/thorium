// project imports
import { GenericJobArgs, RepoDependencyRequest } from '@models/reactions';
import { ToolRunRequest, WorkflowRunRequest } from '@models/runs';
import { parsePositiveInt } from '@utilities/agentic/forms';
import { splitEntries, splitPlainEntries } from '@utilities/runs';

/** The tabs of the launch page, as the `tab` URL parameter names them. */
export enum LaunchTab {
  Workflow = 'workflow',
  Tool = 'tool',
  Pipeline = 'pipeline',
}

/**
 * Read the launch page's tab from its `tab` URL parameter.
 *
 * @param param - The parameter's value, or `null` when the URL has none.
 * @param fallback - The tab for a missing or unknown value, the Workflow tab unless given.
 * @returns The named tab, or the fallback.
 */
export function launchTabFromParam(param: string | null, fallback: LaunchTab = LaunchTab.Workflow): LaunchTab {
  return Object.values(LaunchTab).find((tab) => tab === param) ?? fallback;
}

/** The values every launch form collects before they become a run request. */
export interface LaunchInputs {
  /** The SHA256s of the samples to run on (uploaded files, typed hashes) */
  samples: string[];
  /** The repos to run on */
  repos: RepoDependencyRequest[];
  /** The group to run in, or an empty string to let the API pick */
  group: string;
  /** Tags as typed (comma or newline separated) */
  tags: string;
  /** The SLA in seconds as typed */
  sla: string;
}

/**
 * Check the SLA a launch form collected.
 *
 * A blank SLA is valid (the API picks its default); anything else must be a positive whole number
 * of seconds, because a bad value is otherwise dropped from the request without a word.
 *
 * @param sla - The SLA as typed.
 * @returns A message saying what is wrong, or an empty string when the SLA is blank or valid.
 */
export function slaProblem(sla: string): string {
  // a blank SLA means the API's default applies
  if (sla.trim() === '') return '';
  return parsePositiveInt(sla) === undefined ? 'The SLA must be a positive whole number of seconds, or blank' : '';
}

/** A launch refusal split into the text to show and the trace id to keep for support. */
export interface RunError {
  /** The refusal as a sentence, without markup characters or the trace */
  text: string;
  /** The API's trace id for the failed request, or an empty string when the message has none */
  trace: string;
}

/**
 * Tidy the API's refusal of a run for display.
 *
 * The client formats a failure as `<request>: <reason> trace: <id>`, and the reason quotes names
 * in backticks; neither reads well in a banner, so the trace id is split off and the backticks
 * become quotes.
 *
 * @param message - The error message the client reported.
 * @returns The text to show and the trace id.
 */
export function cleanRunError(message: string): RunError {
  const found = /\s+trace:\s*(\S+)\s*$/.exec(message);
  const trace = found ? found[1] : '';
  const text = (found ? message.slice(0, found.index) : message).replace(/`/g, "'");
  return { text, trace };
}

/**
 * Parse `KEY=VALUE` lines into the keyword args of a job; repeated keys append values.
 *
 * @param text - One `KEY=VALUE` entry per line.
 * @returns The keyword args, or the first malformed line as an error.
 */
export function parseKwargs(text: string): { kwargs: Record<string, string[]>; error: string | null } {
  const kwargs: Record<string, string[]> = {};
  for (const line of text.split('\n').map((entry) => entry.trim())) {
    if (line === '') continue;
    const at = line.indexOf('=');
    // a keyword arg needs a non-empty key
    if (at < 1) return { kwargs: {}, error: `"${line}" must be written as KEY=VALUE` };
    const key = line.slice(0, at);
    kwargs[key] = [...(kwargs[key] ?? []), line.slice(at + 1)];
  }
  return { kwargs, error: null };
}

/**
 * Build the job args of a direct tool run.
 *
 * @param positionals - Positional args, one per line.
 * @param kwargs - Keyword args, `KEY=VALUE` per line.
 * @param switches - Switches, one per line or comma separated.
 * @returns The args, or the reason the keyword args are malformed.
 */
export function buildJobArgs(positionals: string, kwargs: string, switches: string): { args: GenericJobArgs | null; error: string | null } {
  const parsed = parseKwargs(kwargs);
  if (parsed.error) return { args: null, error: parsed.error };
  return {
    args: {
      positionals: positionals
        .split('\n')
        .map((entry) => entry.trim())
        .filter((entry) => entry !== ''),
      kwargs: parsed.kwargs,
      switches: splitEntries(switches),
      opts: { override_positionals: false, override_kwargs: false },
    },
    error: null,
  };
}

/**
 * Build a workflow run request from the launch form values.
 *
 * @param inputs - The collected inputs and settings.
 * @returns The request; unset group and SLA are left out so the API picks defaults.
 */
export function buildWorkflowRequest(inputs: LaunchInputs): WorkflowRunRequest {
  const request: WorkflowRunRequest = { inputs: { samples: inputs.samples, repos: inputs.repos }, tags: splitPlainEntries(inputs.tags) };
  if (inputs.group !== '') request.group = inputs.group;
  const sla = parsePositiveInt(inputs.sla);
  if (sla !== undefined) request.sla = sla;
  return request;
}

/**
 * Build a direct tool run request from the launch form values.
 *
 * @param inputs - The collected inputs and settings.
 * @param args - The job args.
 * @returns The request; unset group and SLA are left out.
 */
export function buildToolRequest(inputs: LaunchInputs, args: GenericJobArgs): ToolRunRequest {
  return { ...buildWorkflowRequest(inputs), args };
}

/**
 * Pick the groups a run may use: the instance's groups the user is in.
 *
 * @param instanceGroups - The groups of the workflow or tool instance.
 * @param userGroups - The groups of the signed in user.
 * @returns The candidate run groups, in the instance's order.
 */
export function candidateGroups(instanceGroups: string[], userGroups: string[]): string[] {
  return instanceGroups.filter((group) => userGroups.includes(group));
}

/**
 * Find the page of a run just launched.
 *
 * The launch response holds only the run's id, so the group comes from the form. An admin may run
 * in an instance group they are not in; the API then picks the group, which is known only when the
 * instance has just one.
 *
 * @param id - The new run's id.
 * @param chosen - The group chosen in the form, or empty when the API picked it.
 * @param candidates - The instance's groups the user is in.
 * @param instanceGroups - Every group of the instance.
 * @returns The run's details page, or the runs list when its group is unknown.
 */
export function runPagePath(id: string, chosen: string, candidates: string[], instanceGroups: string[]): string {
  const group = chosen || candidates[0] || (instanceGroups.length === 1 ? instanceGroups[0] : '');
  return group ? `/runs/${group}/${id}` : '/runs';
}
