// project imports
import { toolVersionLabel } from '@utilities/agentic/display';
import { Pipeline } from '@models/pipelines';
import { Tool, ToolKind } from '@models/tools';

/** The criteria the tool list can be narrowed by. */
export interface ToolFilter {
  /** Free text matched against the name and description */
  text: string;
  /** Only this kind of tool, or an empty string for every kind */
  kind: ToolKind | '';
  /** Only tools in this group, or an empty string for every group */
  group: string;
  /** Hide tools whose every version is deprecated */
  hideDeprecated: boolean;
}

/** A filter that keeps every tool. */
export const NO_TOOL_FILTER: ToolFilter = { text: '', kind: '', group: '', hideDeprecated: false };

/**
 * Narrow a list of tools by a filter.
 *
 * @param tools - The tools to filter.
 * @param filter - The criteria to apply; empty criteria keep everything.
 * @returns The tools that satisfy every criterion, in their original order.
 */
export function filterTools(tools: Tool[], filter: ToolFilter): Tool[] {
  const text = filter.text.trim().toLowerCase();
  return tools.filter((tool) => {
    const meta = tool.metadata.Tool;
    if (filter.kind !== '' && meta.kind !== filter.kind) return false;
    if (filter.group !== '' && !tool.groups.includes(filter.group)) return false;
    // a tool with no live version has nothing left to run
    if (filter.hideDeprecated && meta.latest === null) return false;
    if (text === '') return true;
    const haystack = `${tool.name} ${tool.description ?? ''} ${toolVersionLabel(meta.latest_version)}`.toLowerCase();
    return haystack.includes(text);
  });
}

/**
 * Count how many of a tool's versions are deprecated.
 *
 * @param tool - The tool.
 * @returns The number of deprecated instances.
 */
export function deprecatedCount(tool: Tool): number {
  return (tool.metadata.Tool.instances ?? []).filter((instance) => instance.deprecated).length;
}

/**
 * Find the pipelines' stages backed by a tool, by the tool id or by one of its instance ids.
 *
 * @param stageTools - A pipeline's stage to tool-or-instance id map.
 * @param ids - The tool id and every instance id of the tool.
 * @returns The names of the stages the tool backs.
 */
export function stagesBackedBy(stageTools: Record<string, string> | undefined, ids: string[]): string[] {
  return Object.entries(stageTools ?? {})
    .filter(([, id]) => ids.includes(id))
    .map(([stage]) => stage)
    .sort();
}

/** A pipeline with stages backed by a tool. */
export interface PipelineUse {
  /** The pipeline's name */
  name: string;
  /** The pipeline's group */
  group: string;
  /** The stages the tool backs, sorted */
  stages: string[];
}

/**
 * Find the pipelines with stages backed by a tool, by the tool id or by one of its instance ids.
 *
 * @param pipelines - The pipelines of the tool's groups.
 * @param ids - The tool id and every instance id of the tool.
 * @returns Each pipeline with at least one backed stage, in the order given.
 */
export function pipelineUses(pipelines: Pick<Pipeline, 'name' | 'group' | 'stage_tools'>[], ids: string[]): PipelineUse[] {
  return pipelines.flatMap((pipeline) => {
    const stages = stagesBackedBy(pipeline.stage_tools, ids);
    return stages.length > 0 ? [{ name: pipeline.name, group: pipeline.group, stages }] : [];
  });
}

/**
 * List the groups a tool can be added to from its edit form.
 *
 * @param editable - The groups where the user may add entities, sorted.
 * @param current - The groups the tool is already in.
 * @returns The editable groups the tool is not in yet, in the order given.
 */
export function addableGroups(editable: string[], current: string[]): string[] {
  return editable.filter((group) => !current.includes(group));
}

/**
 * Keep only the chosen groups that are still offered.
 *
 * A group can be chosen while the group details are loading and then drop out of the offered
 * list once they are known; it must not stay selected, since the API would refuse it.
 *
 * @param chosen - The groups the user chose.
 * @param offered - The groups currently offered for choosing.
 * @returns The chosen groups that are in the offered list, in the order chosen.
 */
export function stillOffered(chosen: string[], offered: string[]): string[] {
  return chosen.filter((group) => offered.includes(group));
}
