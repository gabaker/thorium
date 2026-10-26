import { ModelMatcher } from '@models/agents';
import { WorkflowInstanceEntity } from '@models/workflows';

/** Words for each instance trigger variant a workflow or toolset instance can record. */
const TRIGGER_WORDS: Record<string, string> = {
  WorkflowUpdated: 'Workflow changed',
  ToolSetUpdated: 'Toolset updated',
  AgentUpdated: 'Agent changed',
  ToolUpdated: 'Tool updated',
  Manual: 'Requested',
};

/**
 * Describe what caused a workflow or toolset instance build.
 *
 * @param trigger - The serialized trigger: a bare variant name or a single-key object.
 * @returns Words for the trigger, such as `Workflow changed`; an unknown variant is shown as is.
 */
export function triggerText(trigger: unknown): string {
  let variant = 'Unknown';
  if (typeof trigger === 'string') variant = trigger;
  else if (trigger && typeof trigger === 'object') variant = Object.keys(trigger)[0] ?? 'Unknown';
  return TRIGGER_WORDS[variant] ?? variant;
}

/**
 * Describe a default model list as one line.
 *
 * @param models - The matchers, highest priority first.
 * @returns The ids joined with `>` or `none`.
 */
export function defaultModelsText(models: ModelMatcher[] | undefined): string {
  return models && models.length > 0 ? models.map((model) => model.model_id).join(' > ') : 'none';
}

/**
 * Order workflow instances newest revision first.
 *
 * @param instances - The instances to order.
 * @returns A new sorted list.
 */
export function newestFirst<T extends { metadata: { WorkflowInstance: Pick<WorkflowInstanceEntity, 'revision'> } }>(instances: T[]): T[] {
  return [...instances].sort((a, b) => b.metadata.WorkflowInstance.revision - a.metadata.WorkflowInstance.revision);
}

/**
 * Describe a workflow's agent entry for display.
 *
 * @param entry - The entry agent id, or null.
 * @param names - An id to name lookup.
 * @returns The agent's name, the raw id when unknown, or `none`.
 */
export function entryText(entry: string | null | undefined, names: Map<string, string>): string {
  if (!entry) return 'none';
  return names.get(entry) ?? entry;
}

/**
 * Build a default model list from edited model ids, keeping the matcher already stored for an id.
 *
 * A stored matcher can narrow by provider or API, which a list of ids can't show, so an id that
 * stays keeps its whole matcher and only a new id becomes a bare one.
 *
 * @param current - The matchers the workflow has now.
 * @param ids - The model ids after the edit, highest priority first.
 * @returns The matchers to store.
 */
export function matchersForIds(current: ModelMatcher[] | undefined, ids: string[]): ModelMatcher[] {
  const stored = new Map((current ?? []).map((matcher) => [matcher.model_id, matcher]));
  return ids.map((id) => stored.get(id) ?? { model_id: id });
}

/** The levels a workflow's model auth can be limited to; the harness and workflow levels are reserved and nothing supplies them. */
export const MODEL_AUTH_LEVELS = ['Group', 'User'];

/**
 * Combine the model auth levels a user checked with the reserved levels a workflow already has.
 *
 * @param current - The levels the workflow has now.
 * @param checked - The editable levels that are checked.
 * @returns The levels to store; empty means every level may supply auth.
 */
export function mergeAuthLevels(current: string[] | undefined, checked: string[]): string[] {
  // a reserved level isn't offered for editing, so an edit must not drop it
  const reserved = (current ?? []).filter((level) => !MODEL_AUTH_LEVELS.includes(level));
  return [...reserved, ...MODEL_AUTH_LEVELS.filter((level) => checked.includes(level))];
}
