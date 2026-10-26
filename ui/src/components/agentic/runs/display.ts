// project imports
import { Sample } from '@models/files';
import { Reaction } from '@models/reactions';
import { RunKind, RunProvenance } from '@models/runs';
import { parseWorkflowResultKey, runKind } from '@utilities/runs';

// the run status tone is the shared status tone, so a status has one colour on every page
export { statusTone } from '@components/shared/badges/Badge';

/**
 * Keep the runs of one kind, or all runs.
 *
 * @param runs - The runs to filter.
 * @param kind - The kind to keep, or `null` for every kind.
 * @returns The matching runs.
 */
export function filterRunsByKind(runs: Reaction[], kind: RunKind | null): Reaction[] {
  return kind === null ? runs : runs.filter((run) => runKind(run) === kind);
}

/**
 * Merge several lists of runs into one without repeats, latest SLA expiry first.
 *
 * A run records no creation time, so this is the order the group status lists page by; it follows
 * creation time only among runs that share an SLA duration.
 *
 * @param lists - The lists to merge.
 * @returns Each run once (by id), sorted by descending SLA expiry.
 */
export function mergeRuns(lists: Reaction[][]): Reaction[] {
  const byId = new Map<string, Reaction>();
  for (const run of lists.flat()) byId.set(run.id, run);
  // compare instants: the timestamps vary in fractional precision, so they don't sort as text
  return [...byId.values()].sort((a, b) => Date.parse(b.sla) - Date.parse(a.sla));
}

/**
 * Show an API timestamp in the reader's locale.
 *
 * The API writes instants as RFC 3339 text with nanoseconds, which no one reads at a glance.
 *
 * @param text - The timestamp text.
 * @returns The local date and time, or the text unchanged when it is not a date.
 */
export function formatTimestamp(text: string): string {
  const instant = Date.parse(text);
  return Number.isNaN(instant) ? text : new Date(instant).toLocaleString();
}

/**
 * Get the name of an agent of a run.
 *
 * @param provenance - The run's provenance, or `null` when it recorded none.
 * @param agent - The agent's entity id.
 * @returns The agent's name at run time, or its id when the provenance doesn't list it.
 */
export function agentName(provenance: RunProvenance | null, agent: string): string {
  return provenance?.agents.find((entry) => entry.agent === agent)?.name ?? agent;
}

/**
 * Name a workflow run's instance the way the CLI does.
 *
 * @param resultKey - The run's result key, `<workflow>#r<revision>~<instance id>`.
 * @param instance - The run's workflow instance id.
 * @returns `<workflow>#r<revision>` when the key names this instance, otherwise the id.
 */
export function workflowInstanceLabel(resultKey: string, instance: string): string {
  const parsed = parseWorkflowResultKey(resultKey);
  return parsed && parsed.instance === instance ? `${parsed.name}#r${parsed.revision}` : instance;
}

/**
 * List the file names a sample was submitted under.
 *
 * @param sample - The sample's details.
 * @returns The distinct submission names in order, or an empty string when none was named.
 */
export function sampleFileNames(sample: Pick<Sample, 'submissions'>): string {
  // the API leaves a submission's name out when none was given
  const names = (sample.submissions ?? []).map((submission) => submission.name ?? '').filter((name) => name !== '');
  return [...new Set(names)].join(', ');
}
