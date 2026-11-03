// project imports
import { isInstanceReady, toolVersionLabel } from './display';
import { Tool } from '@models/tools';
import { ToolSetInstance } from '@models/toolsets';

/** One version a link can be pinned to. */
export interface PinOption {
  /** The instance id the link names when pinned to this version */
  value: string;
  /** The version as it is shown */
  text: string;
}

/** The versions a link to one root can be pinned to, and how its latest version reads. */
export interface PinChoice {
  /** The root's id, which the link names when it follows latest */
  id: string;
  /** The root's name */
  name: string;
  /** What following latest resolves to now, such as `latest (now 1.2.0)` */
  latestText: string;
  /** The versions the link can be pinned to, newest first */
  options: PinOption[];
}

/**
 * List the versions of a tool a link can pin, with what following latest resolves to now.
 *
 * Deprecated versions are left out since nothing new should pin them.
 *
 * @param tool - The tool, with its instances as the list API returns them.
 * @returns The tool's pin choice.
 */
export function toolPinChoice(tool: Tool): PinChoice {
  const meta = tool.metadata.Tool;
  const latest = toolVersionLabel(meta.latest_version);
  return {
    id: tool.id,
    name: tool.name,
    latestText: latest ? `latest (now ${latest})` : 'latest',
    options: (meta.instances ?? [])
      .filter((instance) => !instance.deprecated)
      .map((instance) => ({ value: instance.id, text: `pinned ${toolVersionLabel(instance.version)}` })),
  };
}

/**
 * List the Ready instances of a toolset a link can pin, with the instance following latest resolves to now.
 *
 * @param id - The toolset's id.
 * @param name - The toolset's name.
 * @param instances - Toolset instances of any toolset; the ones of other toolsets are ignored.
 * @returns The toolset's pin choice, newest revision first.
 */
export function toolSetPinChoice(id: string, name: string, instances: ToolSetInstance[]): PinChoice {
  // an Invalid instance can't be used by an agent, so only Ready ones can be pinned
  const ready = instances
    .filter((item) => item.metadata.ToolSetInstance.tool_set === id && isInstanceReady(item.metadata.ToolSetInstance.status))
    .sort((a, b) => b.metadata.ToolSetInstance.revision - a.metadata.ToolSetInstance.revision);
  return {
    id,
    name,
    latestText: ready.length > 0 ? `latest (now r${ready[0].metadata.ToolSetInstance.revision})` : 'latest',
    options: ready.map((item) => ({ value: item.id, text: `pinned r${item.metadata.ToolSetInstance.revision}` })),
  };
}

/**
 * Turn picked roots into the ids a link form sends: the root for latest or the chosen instance when pinned.
 *
 * @param ids - The picked root ids.
 * @param pins - The chosen instance id for each pinned root; a missing or empty entry follows latest.
 * @returns The ids to send, in the order picked.
 */
export function applyPins(ids: string[], pins: Record<string, string>): string[] {
  return ids.map((id) => pins[id] || id);
}

/**
 * Find the Image tool instance an agentic tool's spec is pinned to.
 *
 * @param spec - The spec of a tool instance, keyed by the tool kind.
 * @returns The pinned instance id of a container backed agentic tool, or null for any other spec.
 */
export function pinnedToolInstance(spec: Record<string, unknown> | null | undefined): string | null {
  // only an agentic spec has a backing, and only a container backing names an Image tool instance
  const backing = (spec?.['Agentic'] as { backing?: { Container?: { tool_instance?: unknown } } } | undefined)?.backing;
  const id = backing?.Container?.tool_instance;
  return typeof id === 'string' && id !== '' ? id : null;
}
