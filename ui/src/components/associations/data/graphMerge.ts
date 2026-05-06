import { Graph } from '@models/trees';

export function mergeGrowthInto(base: Graph, data: Graph, grownNodeIds: string[]): Graph {
  const merged = structuredClone(base);

  if (data.data_map) {
    for (const nodeId of Object.keys(data.data_map)) {
      merged.data_map[nodeId] = data.data_map[nodeId];
    }
  }

  if (data.branches) {
    for (const source of Object.keys(data.branches)) {
      if (source in merged.branches) {
        const existingKeys = new Set(merged.branches[source].map((b) => `${b.node}-${b.direction}-${b.relationship_hash ?? ''}`));
        for (const branch of data.branches[source]) {
          const key = `${branch.node}-${branch.direction}-${branch.relationship_hash ?? ''}`;
          if (!existingKeys.has(key)) {
            merged.branches[source].push(branch);
            existingKeys.add(key);
          }
        }
      } else {
        merged.branches[source] = data.branches[source];
      }
    }
  }

  const grownSet = new Set(grownNodeIds);
  const remaining = merged.growable.filter((id) => !grownSet.has(id));
  if (data.growable) {
    remaining.push(...data.growable);
  }
  merged.growable = remaining;

  return merged;
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
