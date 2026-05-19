import React, { createContext, useContext, useMemo } from 'react';

import { useGraphData } from '@components/associations/data/GraphDataContext';
import type { Sample } from '@models/files';
import { TreeNodeKey, type TreeNode } from '@models/trees';
import type { IncidentSummaryData, NodeTypeCounts, FileExtensionCount } from './types';

const IncidentDataContext = createContext<IncidentSummaryData | undefined>(undefined);

export const useIncidentData = (): IncidentSummaryData => {
  const ctx = useContext(IncidentDataContext);
  if (ctx === undefined) {
    throw new Error('useIncidentData must be used within an IncidentDataProvider');
  }
  return ctx;
};

function extractExtension(name: string): string {
  const dot = name.lastIndexOf('.');
  if (dot < 1) return '(none)';
  return name.substring(dot).toLowerCase();
}

function deriveData(dataMap: Record<string, TreeNode>): Omit<IncidentSummaryData, 'loading' | 'error'> {
  const files: Sample[] = [];
  const entityKindCounts = new Map<string, number>();
  let repoCount = 0;
  let tagCount = 0;
  const extMap = new Map<string, number>();

  const entries = Object.values(dataMap);
  for (const node of entries) {
    if (TreeNodeKey.Sample in node && node.Sample) {
      files.push(node.Sample);
      const submissions = node.Sample.submissions ?? [];
      for (const sub of submissions) {
        if (sub.name) {
          const ext = extractExtension(sub.name);
          extMap.set(ext, (extMap.get(ext) ?? 0) + 1);
        }
      }
    } else if (TreeNodeKey.Repo in node) {
      repoCount++;
    } else if (TreeNodeKey.Tag in node) {
      tagCount++;
    } else if (TreeNodeKey.Entity in node && node.Entity) {
      const kind: string = node.Entity.kind ?? 'Other';
      entityKindCounts.set(kind, (entityKindCounts.get(kind) ?? 0) + 1);
    }
  }

  const fileExtensions: FileExtensionCount[] = Array.from(extMap.entries())
    .map(([extension, count]) => ({ extension, count }))
    .sort((a, b) => b.count - a.count);

  const nodeTypeCounts: NodeTypeCounts = {
    files: files.length,
    repos: repoCount,
    tags: tagCount,
    entities: entityKindCounts,
  };

  return {
    files,
    nodeTypeCounts,
    fileExtensions,
    totalNodes: entries.length,
  };
}

interface IncidentDataProviderProps {
  children: React.ReactNode;
}

export const IncidentDataProvider: React.FC<IncidentDataProviderProps> = ({ children }) => {
  const { graph, graphVersion, loading, error } = useGraphData();

  const derived = useMemo(
    () => deriveData(graph.data_map),
    // graphVersion is bumped whenever graph.data_map changes
    [graphVersion],
  );

  const value = useMemo<IncidentSummaryData>(() => ({ ...derived, loading, error }), [derived, loading, error]);

  return <IncidentDataContext.Provider value={value}>{children}</IncidentDataContext.Provider>;
};
