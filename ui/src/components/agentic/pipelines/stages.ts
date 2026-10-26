// project imports
import { parsePositiveInt } from '@utilities/agentic/forms';
import { Pipeline, PipelineCreate } from '@models/pipelines';
import { toolVersionLabel } from '@utilities/agentic/display';
import { Tool, ToolKind, ToolVersion } from '@models/tools';

/** One stage of a pipeline being edited. */
export interface StageRow {
  /** The stage's name, unique within the pipeline */
  name: string;
  /** The id of the tool that backs the stage */
  tool: string;
  /** The id of the tool instance to pin, or an empty string to follow the tool's latest version */
  instance: string;
}

/** A step of a pipeline: stages that run in parallel, in an ordered list of steps. */
export type StageStep = StageRow[];

/** The values of the pipeline create form. */
export interface PipelineFormValues {
  /** The pipeline's name */
  name: string;
  /** The pipeline's group */
  group: string;
  /** A description */
  description: string;
  /** The SLA in seconds as typed, or an empty string for the default */
  sla: string;
  /** The steps in order */
  steps: StageStep[];
  /** The triggers of the pipeline being copied, passed through unedited */
  triggers?: Pipeline['triggers'];
}

/** A blank stage row. */
export const BLANK_STAGE: StageRow = { name: '', tool: '', instance: '' };

/**
 * Keep a group only when it is one of the groups the form offers.
 *
 * @param group - The group held by the form, which may be seeded from a copied pipeline.
 * @param listed - The groups the user may choose from.
 * @returns The group when it is listed, otherwise an empty string for no group chosen.
 */
export function listedGroup(group: string, listed: string[]): string {
  return listed.includes(group) ? group : '';
}

/**
 * Find the first problem with a pipeline form.
 *
 * @param values - The form values.
 * @returns A message naming the problem, or an empty string when the form can be submitted.
 */
export function pipelineProblem(values: PipelineFormValues): string {
  if (values.name.trim() === '') return 'Enter a name.';
  if (values.group.trim() === '') return 'Choose a group.';
  if (values.steps.length === 0 || values.steps.every((step) => step.length === 0)) return 'Add at least one stage.';
  const seen = new Set<string>();
  for (const step of values.steps) {
    for (const stage of step) {
      const name = stage.name.trim();
      if (name === '') return 'Every stage needs a name.';
      if (seen.has(name)) return `The stage name ${name} is used twice.`;
      seen.add(name);
      if (stage.tool === '') return `Choose a tool for stage ${name}.`;
    }
  }
  const sla = values.sla.trim();
  if (sla !== '' && parsePositiveInt(sla) === undefined) return 'The SLA must be a positive number of seconds.';
  return '';
}

/**
 * Build the pipeline create request from the form.
 *
 * A step with one stage is written as the stage's name and a step with several as a list of names
 * that run in parallel; each stage is backed by its pinned instance or, when none is pinned, by
 * the tool, which follows the tool's latest version. Triggers copied from a source pipeline are
 * sent as they are.
 *
 * @param values - The form values; assumed valid per {@link pipelineProblem}.
 * @returns The request for `POST /pipelines/`.
 */
export function buildPipelineRequest(values: PipelineFormValues): PipelineCreate {
  const steps = values.steps.filter((step) => step.length > 0);
  const order = steps.map((step) => (step.length === 1 ? step[0].name.trim() : step.map((stage) => stage.name.trim())));
  const stageTools: Record<string, string> = {};
  for (const stage of steps.flat()) {
    stageTools[stage.name.trim()] = stageToolRef(stage);
  }
  const request: PipelineCreate = {
    group: values.group,
    name: values.name.trim(),
    order,
    sla: parsePositiveInt(values.sla) ?? 604800,
    stage_tools: stageTools,
  };
  if (values.description.trim() !== '') request.description = values.description.trim();
  // a copy keeps firing on the same events as its source
  if (values.triggers && Object.keys(values.triggers).length > 0) request.triggers = values.triggers;
  return request;
}

/**
 * List the tools that can back a stage of a pipeline in a group.
 *
 * @param tools - Every tool the user can see.
 * @param group - The pipeline's group, or an empty string before one is chosen.
 * @returns The Image tools in the group (every Image tool while no group is chosen), sorted by name;
 *   the API refuses a stage tool from another group.
 */
export function stageToolChoices(tools: Tool[], group: string): Tool[] {
  return tools
    .filter((tool) => tool.metadata.Tool.kind === ToolKind.Image && (group === '' || tool.groups.includes(group)))
    .sort((a, b) => a.name.localeCompare(b.name));
}

/**
 * Get the id a stage's `stage_tools` entry holds.
 *
 * @param stage - The stage row.
 * @returns The pinned instance's id, or the tool's id when the stage follows the tool's latest version.
 */
export function stageToolRef(stage: Pick<StageRow, 'tool' | 'instance'>): string {
  return stage.instance !== '' ? stage.instance : stage.tool;
}

/**
 * Name a tool version in a choice to pin it.
 *
 * A version literally named `latest` is quoted so it is not mistaken for the follow-latest choice.
 *
 * @param version - The tool version to pin.
 * @returns The version's label, in quotes when it is named `latest`.
 */
export function pinnedVersionName(version: ToolVersion | null | undefined): string {
  const label = toolVersionLabel(version);
  return label === 'latest' ? `"${label}"` : label;
}

/** One entry of the pipeline diagram's add-stage menu: a tool to follow at latest or one pinned version. */
export interface StageToolOption {
  /** What the menu shows */
  label: string;
  /** The id the stage's `stage_tools` entry holds: the tool's id or a pinned instance's id */
  value: string;
  /** The tool's name, from which the new stage's name is derived */
  base: string;
}

/**
 * List the entries of the diagram's add-stage menu.
 *
 * Each tool the group's pipelines can use is offered to follow at latest and once per version to pin,
 * the same choices as the pipeline create page's tool and version selects.
 *
 * @param tools - Every tool the user can see.
 * @param group - The pipeline's group.
 * @returns The options, by tool name, each tool's latest entry before its pinned versions.
 */
export function stageToolOptions(tools: Tool[], group: string): StageToolOption[] {
  const options: StageToolOption[] = [];
  const choices = stageToolChoices(tools, group);
  for (const tool of choices) {
    options.push({ label: `${tool.name} · follow latest`, value: tool.id, base: tool.name });
    for (const instance of tool.metadata.Tool.instances ?? []) {
      const deprecated = instance.deprecated ? ' (deprecated)' : '';
      options.push({
        label: `${tool.name} · pin ${pinnedVersionName(instance.version)}${deprecated}`,
        value: instance.id,
        base: tool.name,
      });
    }
  }
  return options;
}

/**
 * Pick a stage name that no other stage of the pipeline uses.
 *
 * @param base - The name to start from, usually the tool's name.
 * @param taken - The stage names already in the pipeline.
 * @returns `base` when it is free, otherwise `base-2`, `base-3`, and so on.
 */
export function uniqueStageName(base: string, taken: Iterable<string>): string {
  const used = new Set(taken);
  if (!used.has(base)) return base;
  let suffix = 2;
  while (used.has(`${base}-${suffix}`)) suffix += 1;
  return `${base}-${suffix}`;
}

/**
 * Unbind the stages whose tool is not offered for a group, keeping the stage names.
 *
 * @param steps - The steps being edited.
 * @param tools - Every tool the user can see.
 * @param group - The group the pipeline is being moved to.
 * @returns The steps with each stage's tool and pinned version kept when the tool can back a stage in the group and cleared otherwise.
 */
export function retainStageTools(steps: StageStep[], tools: Tool[], group: string): StageStep[] {
  const offered = new Set(stageToolChoices(tools, group).map((tool) => tool.id));
  return steps.map((step) => step.map((stage) => (offered.has(stage.tool) ? stage : { ...stage, tool: '', instance: '' })));
}

/**
 * Seed the form's steps from an existing pipeline, for a copy.
 *
 * @param pipeline - The pipeline to copy.
 * @param instanceTool - Maps a tool instance id to its tool id, for stages pinned to an instance.
 * @returns The steps, with stages that have no stage tool left unbound for the user to choose.
 */
export function stepsFromPipeline(pipeline: Pipeline, instanceTool: Record<string, string>): StageStep[] {
  return pipeline.order.map((step) => {
    const names = Array.isArray(step) ? step : [step];
    return names.map((name) => {
      const id = pipeline.stage_tools?.[name] ?? '';
      const tool = instanceTool[id];
      // an id that is a tool instance becomes a pinned stage of its tool
      return tool ? { name, tool, instance: id } : { name, tool: id, instance: '' };
    });
  });
}

/**
 * Turn every stage bound to a tool instance id into a stage of that instance's tool pinned to it.
 *
 * @param steps - The steps being edited.
 * @param instanceTool - Maps a tool instance id to its tool id.
 * @returns The steps with those stages pinned; every other stage is returned unchanged.
 */
export function pinStageInstances(steps: StageStep[], instanceTool: Record<string, string>): StageStep[] {
  return steps.map((step) =>
    step.map((stage) => {
      const tool = instanceTool[stage.tool];
      return stage.instance === '' && tool ? { ...stage, tool, instance: stage.tool } : stage;
    }),
  );
}

/**
 * Tell whether a stage of a pipeline is backed by a tool.
 *
 * @param pipeline - The pipeline.
 * @param stage - The stage's name.
 * @returns `true` when the stage has a stage tool; a stage without one is a legacy image stage.
 */
export function isStageConverted(pipeline: Pick<Pipeline, 'stage_tools'>, stage: string): boolean {
  return pipeline.stage_tools !== undefined && stage in pipeline.stage_tools;
}
