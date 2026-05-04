import React, { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';
import { Seed, Graph, BlankGraph } from '@models/trees';
import { getInitialTree, growTree } from '@thorpi/trees';

interface GraphDataContextType {
  graph: Graph;
  graphId: string;
  graphVersion: number;
  loading: boolean;
  error: string | null;
  growable: Set<string>;
  /** Read the latest Graph directly from the ref — safe inside async callbacks that outlive a render. */
  getGraph: () => Graph;
  grow: (nodeId: string) => Promise<void>;
  growMultiple: (nodeIds: string[], limit?: number) => Promise<void>;
  growToDepth: (depth: number) => Promise<void>;
  reload: (opts?: { filterChildless?: boolean; depth?: number }) => Promise<void>;
}

const GraphDataContext = createContext<GraphDataContextType | undefined>(undefined);

export const useGraphData = (): GraphDataContextType => {
  const context = useContext(GraphDataContext);
  if (context === undefined) {
    throw new Error('useGraphData must be used within a GraphDataProvider');
  }
  return context;
};

function mergeGrowthInto(base: Graph, data: Graph, grownNodeIds: string[]): Graph {
  const merged = structuredClone(base);

  if (data.data_map) {
    for (const nodeId of Object.keys(data.data_map)) {
      merged.data_map[nodeId] = data.data_map[nodeId];
    }
  }

  if (data.branches) {
    for (const source of Object.keys(data.branches)) {
      if (source in merged.branches) {
        merged.branches[source].push(...data.branches[source]);
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

function computeDistances(graph: Graph): Map<string, number> {
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

interface GraphDataProviderProps {
  initial: Seed;
  filterChildless?: boolean;
  depth?: number;
  children: React.ReactNode;
}

export const GraphDataProvider: React.FC<GraphDataProviderProps> = ({
  initial,
  filterChildless = false,
  depth = 1,
  children,
}) => {
  const graphRef = useRef<Graph>(BlankGraph);
  const [graphId, setGraphId] = useState('');
  const [graphVersion, setGraphVersion] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const growChainRef = useRef<Promise<void>>(Promise.resolve());

  const bumpVersion = useCallback(() => setGraphVersion((v) => v + 1), []);

  const handleError = useCallback((err: string) => setError(err), []);

  const fetchInitial = useCallback(
    async (seed: Seed, fc: boolean, d: number) => {
      setLoading(true);
      setError(null);
      const data = await getInitialTree(seed, fc, d, handleError);
      if (data) {
        graphRef.current = data;
        setGraphId(data.id);
        bumpVersion();
      }
      setLoading(false);
    },
    [handleError, bumpVersion],
  );

  useEffect(() => {
    void fetchInitial(initial, filterChildless, depth);
  }, [initial, filterChildless, depth, fetchInitial]);

  const growMultiple = useCallback(
    async (nodeIds: string[], limit = 1) => {
      const id = graphId;
      if (!id) return;

      const doGrow = async () => {
        const data = await growTree(id, nodeIds, handleError, limit);
        if (data) {
          graphRef.current = mergeGrowthInto(graphRef.current, data, nodeIds);
          bumpVersion();
        }
      };

      growChainRef.current = growChainRef.current.then(doGrow, doGrow);
      await growChainRef.current;
    },
    [graphId, handleError, bumpVersion],
  );

  const grow = useCallback(
    async (nodeId: string) => {
      await growMultiple([nodeId]);
    },
    [growMultiple],
  );

  const growToDepth = useCallback(
    async (targetDepth: number) => {
      if (!graphId || targetDepth <= 1) return;

      const doGrowToDepth = async () => {
        let iterations = 0;
        const maxIterations = 20;

        while (iterations < maxIterations) {
          const distances = computeDistances(graphRef.current);
          const growableSet = graphRef.current.growable.map((n) => n.toString());

          const groups = new Map<number, string[]>();
          for (const nodeId of growableSet) {
            const dist = distances.get(nodeId);
            if (dist !== undefined && dist < targetDepth) {
              const limit = targetDepth - dist;
              if (!groups.has(limit)) groups.set(limit, []);
              groups.get(limit)!.push(nodeId);
            }
          }

          if (groups.size === 0) break;

          for (const [limit, nodes] of groups) {
            const data = await growTree(graphId, nodes, handleError, limit);
            if (data) {
              graphRef.current = mergeGrowthInto(graphRef.current, data, nodes);
              bumpVersion();
            }
          }

          iterations++;
        }
      };

      growChainRef.current = growChainRef.current.then(doGrowToDepth, doGrowToDepth);
      await growChainRef.current;
    },
    [graphId, handleError, bumpVersion],
  );

  const reload = useCallback(
    async (opts?: { filterChildless?: boolean; depth?: number }) => {
      await fetchInitial(initial, opts?.filterChildless ?? filterChildless, opts?.depth ?? depth);
    },
    [initial, filterChildless, depth, fetchInitial],
  );

  const growable = useMemo(
    () => new Set(graphRef.current.growable.map((n) => n.toString())),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [graphVersion],
  );

  const getGraph = useCallback(() => graphRef.current, []);

  const value = useMemo<GraphDataContextType>(
    () => ({
      graph: graphRef.current,
      graphId,
      graphVersion,
      loading,
      error,
      growable,
      getGraph,
      grow,
      growMultiple,
      growToDepth,
      reload,
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [graphId, graphVersion, loading, error, growable, getGraph, grow, growMultiple, growToDepth, reload],
  );

  return <GraphDataContext.Provider value={value}>{children}</GraphDataContext.Provider>;
};
