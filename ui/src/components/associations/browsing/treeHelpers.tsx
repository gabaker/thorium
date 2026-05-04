import React from 'react';

import { Graph, TreeNode } from '@models/trees';
import { formatTagNames } from '../utilities';

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
      {Object.keys(tags).reduce((n, k) => n + Object.keys(tags[k]).length, 0) > limit && (
        <span className="preview-tag">...</span>
      )}
    </div>
  );
}

export function findDuplicateNodeIds(graph: Graph): Set<string> {
  const counts = new Map<string, number>();
  const visited = new Set<string>();

  function walk(nodeId: string) {
    counts.set(nodeId, (counts.get(nodeId) ?? 0) + 1);
    if (visited.has(nodeId)) return;
    visited.add(nodeId);
    const children = graph.branches[nodeId];
    if (children) {
      for (const child of children) {
        walk(child.node);
      }
    }
  }

  for (const root of graph.initial) {
    walk(root);
  }

  const duplicates = new Set<string>();
  for (const [id, count] of counts) {
    if (count > 1) duplicates.add(id);
  }
  return duplicates;
}
