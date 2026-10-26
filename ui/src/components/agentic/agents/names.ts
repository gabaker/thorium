import { toolVersionLabel } from '@utilities/agentic/display';
import { AgenticKinds, AnyAgenticEntity } from '@models/agentic';
import { ToolEntity, ToolInstanceEntity } from '@models/tools';
import { ToolSetInstanceEntity } from '@models/toolsets';

/**
 * Build an id to name lookup from entity lists.
 *
 * @param lists - Entity lists whose ids should be resolvable.
 * @returns A map from entity id to name.
 */
export function nameIndex(...lists: AnyAgenticEntity[][]): Map<string, string> {
  const index = new Map<string, string>();
  for (const list of lists) {
    for (const entity of list) index.set(entity.id, entity.name);
  }
  return index;
}

/**
 * Resolve ids to names, keeping an id that has no known name.
 *
 * @param ids - The ids to resolve; anything that is not a string id is skipped.
 * @param index - The id to name lookup.
 * @returns The names in order.
 */
export function resolveNames(ids: unknown[] | null | undefined, index: Map<string, string>): string[] {
  return (ids ?? []).filter((id): id is string => typeof id === 'string').map((id) => index.get(id) ?? id);
}

/**
 * Describe an agent's model list as one line.
 *
 * @param models - The agent's model matchers.
 * @returns Model ids joined in priority order, or `workflow default` when the list is empty.
 */
export function modelsText(models: { model_id: string }[] | null | undefined): string {
  return models && models.length > 0 ? models.map((model) => model.model_id).join(' > ') : 'workflow default';
}

/**
 * Choose the page that shows an entity.
 *
 * @param entity - The entity to link to.
 * @returns The route of its page; kinds without a page link to the tools list.
 */
export function linkFor(entity: Pick<AnyAgenticEntity, 'id' | 'kind'>): string {
  switch (entity.kind) {
    case AgenticKinds.Tool:
      return `/tools/${entity.id}`;
    case AgenticKinds.ToolSet:
      return `/toolsets/${entity.id}`;
    case AgenticKinds.Agent:
      return `/agents/${entity.id}`;
    case AgenticKinds.WorkflowInstance:
      return `/workflows/instances/${entity.id}`;
    case AgenticKinds.WorkflowCatalog:
      return `/workflows/${entity.id}`;
    default:
      return '/tools';
  }
}

/**
 * Say which version of a tool or toolset an agent link resolves to.
 *
 * @param entity - A linked tool or toolset (follows its latest instance) or a tool or toolset instance (pinned).
 * @returns `latest → <version>` for a tool, `latest` for a toolset, `pinned <version>` for a tool instance,
 * `pinned r<revision>` for a toolset instance, or an empty string for other kinds.
 */
export function versionNote(entity: AnyAgenticEntity): string {
  if (entity.kind === AgenticKinds.Tool) {
    const label = toolVersionLabel((entity.metadata.Tool as ToolEntity | undefined)?.latest_version);
    return label ? `latest → ${label}` : 'latest (no version)';
  }
  if (entity.kind === AgenticKinds.ToolInstance) {
    const label = toolVersionLabel((entity.metadata.ToolInstance as ToolInstanceEntity | undefined)?.version);
    return label ? `pinned ${label}` : 'pinned';
  }
  if (entity.kind === AgenticKinds.ToolSet) return 'latest';
  if (entity.kind === AgenticKinds.ToolSetInstance) {
    const revision = (entity.metadata.ToolSetInstance as ToolSetInstanceEntity | undefined)?.revision;
    return revision === undefined ? 'pinned' : `pinned r${revision}`;
  }
  return '';
}
