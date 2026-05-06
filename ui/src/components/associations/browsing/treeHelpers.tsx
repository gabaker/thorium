import React from 'react';

import { Direction, Graph, TreeNode } from '@models/trees';
import { formatTagNames } from '../utilities';

export interface TreeIndex {
  childrenOf: Map<string, string[]>;
  parentsOf: Map<string, string[]>;
}

export function buildTreeIndex(graph: Graph): TreeIndex {
  const childrenOf = new Map<string, string[]>();
  const parentsOf = new Map<string, string[]>();

  const addChild = (parent: string, child: string) => {
    let list = childrenOf.get(parent);
    if (!list) {
      list = [];
      childrenOf.set(parent, list);
    }
    if (!list.includes(child)) list.push(child);

    let parents = parentsOf.get(child);
    if (!parents) {
      parents = [];
      parentsOf.set(child, parents);
    }
    if (!parents.includes(parent)) parents.push(parent);
  };

  if (graph.branches) {
    for (const [nodeId, branches] of Object.entries(graph.branches)) {
      for (const branch of branches) {
        if (branch.direction === Direction.To || branch.direction === Direction.Bidirectional) {
          addChild(nodeId, branch.node);
        } else if (branch.direction === Direction.From) {
          addChild(branch.node, nodeId);
        }
      }
    }
  }

  return { childrenOf, parentsOf };
}

const NODE_TYPE_LABELS: Record<string, string> = {
  file: 'File',
  repo: 'Repository',
  tag: 'Tag',
  device: 'Device',
  vendor: 'Vendor',
  collection: 'Collection',
  filesystem: 'File System',
  folder: 'Folder',
  other: 'Other',
};

export { NODE_TYPE_LABELS };

export function getNodePreviewData(nodeData: TreeNode) {
  if ('Sample' in nodeData && nodeData.Sample) {
    const s = nodeData.Sample;
    return {
      type: 'File',
      fields: [
        { label: 'SHA256', value: s.sha256 ? s.sha256.substring(0, 16) + '...' : undefined },
        { label: 'MD5', value: s.md5 },
        { label: 'Submissions', value: String(s.submissions?.length ?? 0) },
      ],
      tags: s.tags,
    };
  }
  if ('Repo' in nodeData && nodeData.Repo) {
    const r = nodeData.Repo;
    return {
      type: 'Repository',
      fields: [
        { label: 'URL', value: r.url },
        { label: 'Provider', value: r.provider },
      ],
      tags: r.tags,
    };
  }
  if ('Tag' in nodeData && nodeData.Tag) {
    const tagStr = formatTagNames(nodeData.Tag.tags, false);
    return {
      type: 'Tag',
      fields: [{ label: 'Tags', value: tagStr }],
      tags: undefined,
    };
  }
  if ('Entity' in nodeData && nodeData.Entity) {
    const e = nodeData.Entity;
    return {
      type: NODE_TYPE_LABELS[e.kind.toLowerCase()] ?? e.kind,
      fields: [
        { label: 'Name', value: e.name },
        { label: 'Kind', value: e.kind },
        ...(e.description ? [{ label: 'Description', value: e.description }] : []),
      ],
      tags: e.tags,
    };
  }
  return { type: 'Unknown', fields: [], tags: undefined };
}

export function renderTagPreview(tags: Record<string, Record<string, string[]>> | undefined, limit = 8) {
  if (!tags) return null;
  const entries: { key: string; value: string }[] = [];
  for (const key of Object.keys(tags)) {
    for (const value of Object.keys(tags[key])) {
      entries.push({ key, value });
      if (entries.length >= limit) break;
    }
    if (entries.length >= limit) break;
  }
  if (entries.length === 0) return null;
  return (
    <div className="preview-tags">
      {entries.map((t, i) => (
        <span key={i} className="preview-tag">
          {t.key}: {t.value}
        </span>
      ))}
      {Object.keys(tags).reduce((n, k) => n + Object.keys(tags[k]).length, 0) > limit && <span className="preview-tag">...</span>}
    </div>
  );
}

export function findMultiParentNodeIds(graph: Graph, index?: TreeIndex): Set<string> {
  const idx = index ?? buildTreeIndex(graph);
  const multiParent = new Set<string>();
  for (const [nodeId, parents] of idx.parentsOf) {
    if (parents.length > 1) multiParent.add(nodeId);
  }
  return multiParent;
}
