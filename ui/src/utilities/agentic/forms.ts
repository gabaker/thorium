import { parse as parseYaml } from 'yaml';

// project imports
import { AgentCreateForm, ModelMatcher } from '@models/agents';
import { ToolCreateForm, ToolKind, ToolPublishForm } from '@models/tools';
import { ToolSetCreateForm } from '@models/toolsets';
import { WorkflowCreateForm } from '@models/workflows';
import { AgenticKinds } from '@models/agentic';
import { withArticle } from '@utilities/agentic/display';

/** The result of reading a spec document typed by a user. */
export interface ParsedSpec {
  /** The spec as the JSON text the API reads, or `null` when the document is invalid */
  json: string | null;
  /** The kind named by the spec's top-level key, when it has exactly one known key */
  kind: ToolKind | null;
  /** Why the document is invalid, or `null` when it is valid */
  error: string | null;
}

/**
 * Read a positive whole number typed in a form.
 *
 * @param text - The typed text.
 * @returns The number, or `undefined` when it is blank or not a positive integer.
 */
export function parsePositiveInt(text: string): number | undefined {
  const trimmed = text.trim();
  // only digits make a whole number; parseInt alone would read '1e3' or '1,000' as 1
  if (!/^\d+$/.test(trimmed)) return undefined;
  const value = Number.parseInt(trimmed, 10);
  return value > 0 ? value : undefined;
}

/**
 * Check a number field that must be left blank or hold a positive whole number.
 *
 * @param text - The typed text.
 * @returns A message saying what is wrong, or an empty string when the field is blank or valid.
 */
export function positiveIntError(text: string): string {
  // a blank field means the setting is left unset, which is valid
  if (text.trim() === '') return '';
  return parsePositiveInt(text) === undefined ? 'Enter a positive whole number, or leave this blank' : '';
}

/**
 * Read a tool spec written as YAML or JSON and re-encode it as the JSON text the API expects.
 *
 * YAML is a superset of JSON, so one parser reads both. The kind is the spec's one top-level key
 * (`Image`, `Agentic`, or `Harness`).
 *
 * @param text - The spec document.
 * @returns The JSON text and kind, or the reason the document is not a spec.
 */
export function parseToolSpec(text: string): ParsedSpec {
  // an empty document can't be a spec
  if (text.trim() === '') {
    return { json: null, kind: null, error: 'The spec is empty' };
  }
  let value: unknown;
  try {
    value = parseYaml(text);
  } catch (err) {
    return { json: null, kind: null, error: `The spec is not valid YAML or JSON: ${(err as Error).message}` };
  }
  // a spec is a mapping with exactly one top-level key naming its kind
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    return { json: null, kind: null, error: 'The spec must be a mapping with one top-level key: Image, Agentic, or Harness' };
  }
  const keys = Object.keys(value);
  const kinds = Object.values(ToolKind) as string[];
  if (keys.length !== 1 || !kinds.includes(keys[0])) {
    return {
      json: null,
      kind: null,
      error: `The spec must have exactly one top-level key of ${kinds.join(', ')}; found ${keys.join(', ') || 'none'}`,
    };
  }
  return { json: JSON.stringify(value), kind: keys[0] as ToolKind, error: null };
}

/**
 * Prefix a tool kind with the article that reads right before it.
 *
 * @param kind - The kind to name.
 * @returns The kind with `a` or `an` in front, such as `an Agentic` or `a Harness`.
 */
export function kindWithArticle(kind: ToolKind): string {
  return withArticle(kind);
}

/**
 * Describe a spec whose kind differs from the kind it was expected to be.
 *
 * @param specKind - The kind the pasted spec declares.
 * @param expected - What the spec was expected to match, such as `this is an Image tool`.
 * @returns The problem sentence, such as `The spec is an Agentic spec but this is an Image tool`.
 */
export function specKindMismatch(specKind: ToolKind, expected: string): string {
  return `The spec is ${kindWithArticle(specKind)} spec but ${expected}`;
}

/**
 * Print a spec object as YAML-compatible JSON text for an editor to start from.
 *
 * @param spec - The spec object of a tool instance.
 * @returns Indented JSON text, which is also valid YAML.
 */
export function specToText(spec: Record<string, unknown>): string {
  return JSON.stringify(spec, null, 2);
}

/**
 * Append every non-empty value of a list to a form under one repeated field name.
 *
 * @param form - The form to add to.
 * @param field - The repeated field name (for example `groups[]`).
 * @param values - The values to add; blank entries are skipped.
 */
function appendAll(form: FormData, field: string, values: string[]): void {
  // skip blank entries so empty input rows never reach the API
  for (const value of values) {
    if (value.trim() !== '') {
      form.append(field, value.trim());
    }
  }
}

/**
 * Add the fields every entity create form shares: name, groups, and description.
 *
 * @param form - The form to add to.
 * @param kind - The kind of entity being created.
 * @param name - The entity's name.
 * @param groups - The groups to create it in.
 * @param description - The description; omitted when blank.
 */
function addRootFields(form: FormData, kind: AgenticKinds, name: string, groups: string[], description: string): void {
  form.append('kind', kind);
  form.append('name', name.trim());
  appendAll(form, 'groups[]', groups);
  // a blank description is left unset
  if (description.trim() !== '') {
    form.append('description', description.trim());
  }
}

/**
 * Build the multipart form that creates a tool and its first version.
 *
 * @param input - The tool create form values; `spec` must already be JSON text.
 * @returns The form for `POST /entities/`.
 */
export function buildToolCreateForm(input: ToolCreateForm): FormData {
  const form = new FormData();
  addRootFields(form, AgenticKinds.Tool, input.name, input.groups, input.description);
  form.append('metadata[tool_kind]', input.kind);
  form.append('metadata[tool_version]', input.version.trim());
  form.append('metadata[tool_spec]', input.spec);
  return form;
}

/**
 * Build the multipart form that publishes a new version through a tool update.
 *
 * @param input - The version label and JSON spec text of the new version.
 * @returns The form for `PATCH /entities/{tool}`.
 */
export function buildToolPublishForm(input: Pick<ToolPublishForm, 'version' | 'spec'>): FormData {
  const form = new FormData();
  form.append('metadata[tool_version]', input.version.trim());
  form.append('metadata[tool_spec]', input.spec);
  return form;
}

/**
 * Build the multipart form that deprecates or restores a tool instance.
 *
 * @param deprecated - Whether the instance should be deprecated.
 * @returns The form for `PATCH /entities/{instance}`.
 */
export function buildDeprecateForm(deprecated: boolean): FormData {
  const form = new FormData();
  form.append('metadata[tool_instance_deprecated]', String(deprecated));
  return form;
}

/**
 * Build the multipart form that updates an entity's root fields (name, description, groups).
 *
 * @param change - The fields to change; unset fields are not sent.
 * @returns The form for `PATCH /entities/{id}`.
 */
export function buildRootUpdateForm(change: {
  name?: string;
  description?: string;
  clearDescription?: boolean;
  addGroups?: string[];
  removeGroups?: string[];
}): FormData {
  const form = new FormData();
  if (change.name !== undefined && change.name.trim() !== '') form.append('name', change.name.trim());
  if (change.description !== undefined && change.description.trim() !== '') form.append('description', change.description.trim());
  if (change.clearDescription) form.append('clear_description', 'true');
  appendAll(form, 'add_groups[]', change.addGroups ?? []);
  appendAll(form, 'remove_groups[]', change.removeGroups ?? []);
  return form;
}

/**
 * Check a toolset selector's two fields, which must both be filled or both be blank.
 *
 * @param key - The typed selector key.
 * @param value - The typed selector value.
 * @returns A message saying which half is missing, or an empty string when the selector is complete or blank.
 */
export function selectorError(key: string, value: string): string {
  const hasKey = key.trim() !== '';
  const hasValue = value.trim() !== '';
  // a lone half would be left out of the request, so say which one is missing
  if (hasKey && !hasValue) return 'Enter a selector value too, or clear the key';
  if (hasValue && !hasKey) return 'Enter a selector key too, or clear the value';
  return '';
}

/**
 * Build the multipart form that creates a toolset.
 *
 * @param input - The toolset form values.
 * @returns The form for `POST /entities/`.
 */
export function buildToolSetCreateForm(input: ToolSetCreateForm): FormData {
  const form = new FormData();
  addRootFields(form, AgenticKinds.ToolSet, input.name, input.groups, input.description);
  appendAll(form, 'metadata[toolset_tools][]', input.tools);
  // a selector needs both its key and its value
  if (input.selectorKey.trim() !== '' && input.selectorValue.trim() !== '') {
    form.append('metadata[toolset_selector_key]', input.selectorKey.trim());
    form.append('metadata[toolset_selector_value]', input.selectorValue.trim());
  }
  form.append('metadata[toolset_read_only]', String(input.readOnly));
  form.append('metadata[toolset_auto_instance]', String(input.autoInstance));
  return form;
}

/**
 * Build the agent config JSON from the agent form's values, leaving unset settings out.
 *
 * @param input - The agent form values.
 * @returns The config object the API's `agent_config` field carries.
 */
export function buildAgentConfig(input: AgentCreateForm): Record<string, unknown> {
  const config: Record<string, unknown> = {};
  if (input.systemPrompt.trim() !== '') config['system_prompt'] = input.systemPrompt;
  if (input.prompt.trim() !== '') config['prompt'] = input.prompt;
  const models = input.models.map((model) => model.trim()).filter((model) => model !== '');
  if (models.length > 0) config['models'] = models.map((model_id) => ({ model_id }));
  const turns = parsePositiveInt(input.maxTurns);
  if (turns !== undefined) config['max_turns'] = turns;
  // an agent another agent delegates to needs both, so the delegating model can call it
  const functionName = input.functionName?.trim() ?? '';
  if (functionName !== '') config['function_name'] = functionName;
  const modelDescription = input.modelDescription?.trim() ?? '';
  if (modelDescription !== '') config['model_description'] = modelDescription;
  return config;
}

/**
 * Build the multipart form that creates an agent and its links.
 *
 * @param input - The agent form values.
 * @returns The form for `POST /entities/`.
 */
export function buildAgentCreateForm(input: AgentCreateForm): FormData {
  const form = new FormData();
  addRootFields(form, AgenticKinds.Agent, input.name, input.groups, input.description);
  form.append('metadata[agent_config]', JSON.stringify(buildAgentConfig(input)));
  if (input.harness.trim() !== '') form.append('metadata[agent_harness]', input.harness.trim());
  appendAll(form, 'metadata[agent_tools][]', input.tools);
  appendAll(form, 'metadata[agent_tool_sets][]', input.toolSets);
  appendAll(form, 'metadata[agent_sub_agents][]', input.subAgents);
  return form;
}

/**
 * Build the multipart form that creates a workflow.
 *
 * @param input - The workflow form values.
 * @returns The form for `POST /entities/`.
 */
export function buildWorkflowCreateForm(input: WorkflowCreateForm): FormData {
  const form = new FormData();
  addRootFields(form, AgenticKinds.WorkflowCatalog, input.name, input.groups, input.description);
  appendAll(form, 'metadata[workflow_agents][]', input.agents);
  if (input.entryAgent.trim() !== '') form.append('metadata[workflow_entry_agent]', input.entryAgent.trim());
  form.append('metadata[workflow_auto_instance]', String(input.autoInstance));
  const defaults = input.defaultModels.map((model) => model.trim()).filter((model) => model !== '');
  if (defaults.length > 0) {
    form.append('metadata[workflow_default_models]', JSON.stringify(defaults.map((model_id) => ({ model_id }))));
  }
  const alive = parsePositiveInt(input.maxAliveSubAgents);
  if (alive !== undefined) {
    form.append('metadata[workflow_limits]', JSON.stringify({ max_alive_sub_agents: alive }));
  }
  return form;
}

/**
 * Build the multipart form that edits a workflow's agents and settings.
 *
 * The limits, default models, and model auth levels each replace the whole stored value, so a
 * limit set to `null` or an empty model list clears it.
 *
 * @param change - The edits to send; unset edits are left out.
 * @returns The form for `PATCH /entities/{workflow}`.
 */
export function buildWorkflowUpdateForm(change: {
  addAgents?: string[];
  removeAgents?: string[];
  entryAgent?: string;
  clearEntryAgent?: boolean;
  autoInstance?: boolean;
  maxAliveSubAgents?: number | null;
  defaultModels?: ModelMatcher[];
  modelAuthLevels?: string[];
}): FormData {
  const form = new FormData();
  appendAll(form, 'metadata[add_workflow_agents][]', change.addAgents ?? []);
  appendAll(form, 'metadata[remove_workflow_agents][]', change.removeAgents ?? []);
  if (change.entryAgent && change.entryAgent.trim() !== '') form.append('metadata[workflow_entry_agent]', change.entryAgent.trim());
  if (change.clearEntryAgent) form.append('metadata[clear_workflow_entry_agent]', 'true');
  if (change.autoInstance !== undefined) form.append('metadata[workflow_auto_instance]', String(change.autoInstance));
  // an unset limit is sent as an empty object so it replaces the stored one
  if (change.maxAliveSubAgents !== undefined) {
    const limits = change.maxAliveSubAgents === null ? {} : { max_alive_sub_agents: change.maxAliveSubAgents };
    form.append('metadata[workflow_limits]', JSON.stringify(limits));
  }
  if (change.defaultModels !== undefined) form.append('metadata[workflow_default_models]', JSON.stringify(change.defaultModels));
  if (change.modelAuthLevels !== undefined)
    form.append('metadata[workflow_model_auth]', JSON.stringify({ levels: change.modelAuthLevels }));
  return form;
}
