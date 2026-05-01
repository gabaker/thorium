import { Graph } from '@models/trees';

export function mergeGrowthInto(base: Graph, data: Graph, grownNodeIds: string[]): Graph {
  const mergedDataMap = { ...base.data_map };
  if (data.data_map) {
    for (const nodeId of Object.keys(data.data_map)) {
      mergedDataMap[nodeId] = data.data_map[nodeId];
    }
  }

  const mergedBranches = { ...base.branches };
  if (data.branches) {
    for (const source of Object.keys(data.branches)) {
      if (source in mergedBranches) {
        const existing = mergedBranches[source];
        const existingKeys = new Set(existing.map((b) => `${b.node}-${b.direction}-${b.relationship_hash ?? ''}`));
        const newBranches = data.branches[source].filter((branch) => {
          const key = `${branch.node}-${branch.direction}-${branch.relationship_hash ?? ''}`;
          return !existingKeys.has(key);
        });
        if (newBranches.length > 0) {
          mergedBranches[source] = [...existing, ...newBranches];
        }
      } else {
        mergedBranches[source] = data.branches[source];
      }
    }
  }

  const grownSet = new Set(grownNodeIds);
  const remaining = base.growable.filter((id) => !grownSet.has(id));
  if (data.growable) {
    remaining.push(...data.growable);
  }

  return {
    ...base,
    data_map: mergedDataMap,
    branches: mergedBranches,
    growable: remaining,
  };
}

export function computeDistances(graph: Graph): Map<string, number> {
  const distances = new Map<string, number>();
  const queue: [string, number][] = [];

  for (const id of graph.initial) {
    const nodeId = id.toString();
    if (!distances.has(nodeId)) {
      distances.set(nodeId, 0);
      queue.push([nodeId, 0]);
    }
  }

  const adj = new Map<string, Set<string>>();
  const addEdge = (a: string, b: string) => {
    if (!adj.has(a)) adj.set(a, new Set());
    if (!adj.has(b)) adj.set(b, new Set());
    adj.get(a)!.add(b);
    adj.get(b)!.add(a);
  };
  for (const nodeKey of Object.keys(graph.branches)) {
    for (const branch of graph.branches[nodeKey]) {
      addEdge(nodeKey, branch.node.toString());
    }
  }

  let idx = 0;
  while (idx < queue.length) {
    const [current, dist] = queue[idx++];
    const neighbors = adj.get(current);
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
