// spec: ./SPEC.md

// project imports
import type { TreeNode } from '@models/trees';

/**
 * Resolve the dashboard's counted node set from a graph's `data_map` and the entity browser's visible-id set.
 *
 * Both the stats charts and the tags tile must count over the **same** node set for their downselect to
 * agree (a SPEC invariant): the visible ids when a filter is active, otherwise every node in the graph.
 * Centralizing the resolution here keeps the two tiles from drifting apart.
 *
 * @param dataMap - The graph's `data_map` (`{ [nodeId]: TreeNode }`); may be undefined before the graph loads.
 * @param visibleSet - The entity browser's visible-id set when a filter is active, or `null`/`undefined` for
 *   "no filter" (count the whole `data_map`).
 * @returns The node objects to count over, with any ids missing from `data_map` dropped.
 */
export function visibleNodes(dataMap: Record<string, TreeNode> | undefined, visibleSet: Set<string> | null | undefined): TreeNode[] {
  const map = dataMap ?? {};
  // when a filter is active, resolve only the visible ids (dropping any that aren't in the map yet);
  // otherwise count every node in the graph
  return visibleSet
    ? Array.from(visibleSet, (id) => map[id]).filter((node): node is NonNullable<typeof node> => node !== undefined)
    : Object.values(map);
}
