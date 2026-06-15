// spec: ./GraphDataContext.spec.md

import { Graph } from '@models/trees';

export function mergeGrowthInto(initial: Graph, grown: Graph, grownNodeIds: string[]): Graph {
  // DEBUG (remove after diagnosing cross-tree/400 bugs): merging a response from a different tree id than the
  // live graph means a StrictMode/reload race produced a second tree; the merged graph then holds ids the
  // live server tree lacks (the source of "not a valid growable node" 400s).
  if (initial.id && grown.id && initial.id !== grown.id) {
    console.warn('[merge-debug] mergeGrowthInto across DIFFERENT tree ids', { initialTree: initial.id, grownTree: grown.id, grownNodeIds });
  }
  const mergedDataMap = { ...initial.data_map };
  // merge nodes from grown graph into initial
  // override in case we get info for an existing node
  if (grown.data_map) {
    for (const nodeId of Object.keys(grown.data_map)) {
      mergedDataMap[nodeId] = grown.data_map[nodeId];
    }
  }
  // merge branches
  const mergedBranches = { ...initial.branches };
  if (grown.branches) {
    for (const source of Object.keys(grown.branches)) {
      if (source in mergedBranches) {
        const existing = mergedBranches[source];
        const existingKeys = new Set(existing.map((b) => `${b.node}-${b.direction}-${b.relationship_hash ?? ''}`));
        const newBranches = grown.branches[source].filter((branch) => {
          const key = `${branch.node}-${branch.direction}-${branch.relationship_hash ?? ''}`;
          return !existingKeys.has(key);
        });
        if (newBranches.length > 0) {
          mergedBranches[source] = [...existing, ...newBranches];
        }
      } else {
        mergedBranches[source] = grown.branches[source];
      }
    }
  }
  // get unique set of nodes that are still growable
  const grownSet = new Set(grownNodeIds);
  // DEBUG (remove after diagnosing re-send storm): grownNodeIds are strings, but initial.growable may hold
  // JS numbers for short (<10^15) hashes, so the filter below fails to remove a grown numeric id
  // (number !== string) and it gets re-sent on the next growToDepth pass. This logs those ids.
  const notRemovedDueToType = (initial.growable as unknown[]).filter((id) => typeof id !== 'string' && grownSet.has(String(id)));
  if (notRemovedDueToType.length) {
    console.warn('[merge-debug] grown ids NOT removed from growable (number/string mismatch)', { notRemovedDueToType, grownNodeIds });
  }
  // in the future when we page the returned values growing a node we may need to update this logic
  const remaining = initial.growable.filter((id) => !grownSet.has(id));
  if (grown.growable) {
    remaining.push(...grown.growable);
  }
  return {
    ...initial,
    data_map: mergedDataMap,
    branches: mergedBranches,
    growable: remaining,
  };
}

// compute distance from seed nodes to each other node in the graph
// used for "growToDepth" to calculate which growable nodes need to be grown
export function computeDistances(graph: Graph): Map<string, number> {
  const distances = new Map<string, number>();
  const queue: [string, number][] = [];
  // seed distances with 0
  for (const id of graph.initial) {
    const nodeId = id.toString();
    if (!distances.has(nodeId)) {
      distances.set(nodeId, 0);
      queue.push([nodeId, 0]);
    }
  }
  // build undirected adjacency list from branches
  const adjacency = new Map<string, Set<string>>();
  const addEdge = (a: string, b: string) => {
    if (!adjacency.has(a)) adjacency.set(a, new Set());
    if (!adjacency.has(b)) adjacency.set(b, new Set());
    adjacency.get(a)!.add(b);
    adjacency.get(b)!.add(a);
  };
  for (const branchStartNode of Object.keys(graph.branches)) {
    for (const branchNode of graph.branches[branchStartNode]) {
      addEdge(branchStartNode, branchNode.node.toString());
    }
  }
  // BFS traversal
  let idx = 0;
  while (idx < queue.length) {
    const [current, dist] = queue[idx++];
    const neighbors = adjacency.get(current);
    if (!neighbors) continue;
    for (const neighbor of neighbors) {
      if (!distances.has(neighbor)) {
        distances.set(neighbor, dist + 1);
        queue.push([neighbor, dist + 1]);
      }
    }
  }
  return distances;
}
