import React, { useState, useMemo, useCallback } from 'react';
import { asyncDataLoaderFeature, hotkeysCoreFeature, selectionFeature } from '@headless-tree/core';
import { useTree } from '@headless-tree/react';
import cn from 'classnames';
import styled from 'styled-components';
import { OverlayTrigger, Popover, Spinner } from 'react-bootstrap';
import { ErrorBoundary } from 'react-error-boundary';

import RenderErrorAlert from '../../shared/alerts/RenderErrorAlert';
import { BranchNode, Graph, TreeNode } from '@models/trees';
import { getNodeName, formatTagNames } from '../utilities';
import { classifyNode } from '../graph-d3/data';
import { useGraphData } from '../data';
import ArrowSVG from '@assets/icons/arrow_drop_up.svg';

export const ArrowIcon = ArrowSVG.replace('REPLACEME', '64cc66');

const NODE_TYPE_COLORS: Record<string, string> = {
  file: '#f1d592',
  repo: '#f03c2e',
  tag: '#427d8c',
  device: '#ed9624',
  vendor: '#8f30b8',
  collection: '#8f30b8',
  filesystem: '#8f30b8',
  folder: '#f1d592',
  other: '#cacfca',
};

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

const Tree = styled.div`
  .tree button[role='treeitem'] {
    display: flex;
    background: transparent;
    border: none;
    width: 100%;
    padding: 0 0 2px 0;
  }

  .treeitem {
    width: 100%;
    text-align: left;
    color: var(--thorium-text);
    padding: 6px 10px;
    position: relative;
    border-radius: 8px;
    transition: background-color 0.2s ease, outline-color 0.2s ease;
    cursor: pointer;
  }
  .treeitem:hover {
    background-color: rgb(0, 102, 255, 0.1);
    color: var(--thorium-text);
    border-color: black;
  }

  .tree button[role='treeitem']:focus {
    outline: none;
  }

  button:focus-visible .treeitem.focused,
  .treeitem.searchmatch.focused {
    outline: 2px solid black;
  }

  .treeitem.drop {
    border-color: var(--selected-color);
    background-color: #e1f1f8;
  }

  .treeitem.searchmatch {
    background-color: #e1f8ff;
  }

  .treeitem.folder:before {
    content: url(data:image/svg+xml;base64,PHN2ZyB2ZXJzaW9uPSIxLjEiIHhtbG5zPSJodHRwOi8vd3d3LnczLm9yZy8yMDAwL3N2ZyIgeG1sbnM6eGxpbms9Imh0dHA6Ly93d3cudzMub3JnLzE5OTkveGxpbmsiIHg9IjBweCIgeT0iMHB4IiB2aWV3Qm94PSIwIDAgMTYgMTYiIGVuYWJsZS1iYWNrZ3JvdW5kPSJuZXcgMCAwIDE2IDE2IiB4bWw6c3BhY2U9InByZXNlcnZlIj48Zz48Zz48cGF0aCBmaWxsLXJ1bGU9ImV2ZW5vZGQiIGNsaXAtcnVsZT0iZXZlbm9kZCIgZD0iTTQuNjQ2IDEuNjQ2YS41LjUgMCAwIDEgLjcwOCAwbDYgNmEuNS41IDAgMCAxIDAgLjcwOGwtNiA2YS41LjUgMCAwIDEtLjcwOC0uNzA4TDEwLjI5MyA4IDQuNjQ2IDIuMzU0YS41LjUgMCAwIDEgMC0uNzA4eiIgY2xhc3M9InJjdC10cmVlLWl0ZW0tYXJyb3ctcGF0aCI+PC9wYXRoPjwvZz48L2c+PC9zdmc+);
    background-color: white;
    width: 10px;
    display: inline-block;
    z-index: 1;
    margin-right: 4px;
    transition: transform 0.1s ease-in-out;
  }

  .treeitem.folder.expanded:before {
    transform: rotate(90deg);
  }

  .treeitem:not(.folder) {
    padding-left: 24px;
  }

  .treeitem.selected:after {
    content: ' ';
    position: absolute;
    top: 5px;
    left: -2px;
    height: 24px;
    width: 4px;
    background-color: #0366d6;
    border-radius: 99px;
  }

  .treeitem.duplicate-highlight {
    outline: 2px dashed #e8a838;
    outline-offset: -2px;
    background-color: rgba(232, 168, 56, 0.12);
  }

  .treeitem.duplicate-highlight:hover {
    background-color: rgba(232, 168, 56, 0.2);
  }

  .outeritem {
    display: flex;
    align-items: center;
    gap: 2px;
  }
  .outeritem button:not([role='treeitem']) {
    padding: 2px 4px;
    height: 80%;
  }

  .node-type-badge {
    display: inline-block;
    font-size: 0.65rem;
    font-weight: 600;
    padding: 1px 5px;
    border-radius: 4px;
    margin-right: 6px;
    color: #fff;
    vertical-align: middle;
    letter-spacing: 0.3px;
  }

  .duplicate-indicator {
    display: inline-block;
    font-size: 0.6rem;
    padding: 0 4px;
    margin-left: 4px;
    border-radius: 3px;
    background-color: rgba(232, 168, 56, 0.25);
    color: #b07d1a;
    vertical-align: middle;
  }
`;

const PreviewPopover = styled(Popover)`
  --bs-popover-max-width: 360px;

  .popover-body {
    padding: 10px 14px;
    font-size: 0.82rem;
  }

  .preview-type {
    font-weight: 600;
    margin-bottom: 4px;
  }

  .preview-field {
    margin-bottom: 2px;
    color: var(--thorium-text-secondary, #666);
  }

  .preview-field strong {
    color: var(--thorium-text, #333);
  }

  .preview-tags {
    display: flex;
    flex-wrap: wrap;
    gap: 3px;
    margin-top: 6px;
  }

  .preview-tag {
    font-size: 0.7rem;
    padding: 1px 6px;
    border-radius: 4px;
    background-color: rgba(66, 125, 140, 0.15);
    color: #427d8c;
  }

  .preview-duplicate-warn {
    margin-top: 6px;
    padding: 3px 6px;
    font-size: 0.7rem;
    border-radius: 4px;
    background-color: rgba(232, 168, 56, 0.15);
    color: #b07d1a;
  }
`;

function getNodePreviewData(nodeData: TreeNode) {
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

function renderTagPreview(tags: Record<string, Record<string, string[]>> | undefined, limit = 8) {
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

function findDuplicateNodeIds(graph: Graph): Set<string> {
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

const AssociationTreeComponent: React.FC = () => {
  const { graph, graphVersion, grow, growable } = useGraphData();
  const [loadingItemData, setLoadingItemData] = useState<string[]>([]);
  const [loadingItemChildrens, setLoadingItemChildrens] = useState<string[]>([]);
  const [highlightedNodeId, setHighlightedNodeId] = useState<string | null>(null);

  const duplicateNodes = useMemo(() => {
    if (!graph.id) return new Set<string>();
    return findDuplicateNodeIds(graph);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graphVersion]);

  const tree = useTree<string>({
    state: { loadingItemData, loadingItemChildrens },
    setLoadingItemData,
    setLoadingItemChildrens,
    rootItemId: 'root',
    getItemName: (node) => {
      const nodeId = node.getId();
      if (graph.data_map && nodeId in graph.data_map) {
        return getNodeName(graph.data_map[nodeId], 100);
      }
      return node.getItemData();
    },
    isItemFolder: (node) => {
      const nodeId = node.getId();
      const branches = graph.branches ? Object.keys(graph.branches) : [];
      return !!(growable.has(nodeId) || branches.includes(nodeId));
    },
    createLoadingItemData: () => 'loading...',
    dataLoader: {
      getItem: (nodeId) => nodeId,
      getChildren: async (nodeId) => {
        if (nodeId == 'root') {
          return graph.initial;
        }

        if (growable.has(nodeId)) {
          await grow(nodeId);
        }

        const children: string[] = [];
        if (graph.branches && nodeId in graph.branches) {
          graph.branches[nodeId].forEach((node: BranchNode) => children.push(node.node));
        }
        return children;
      },
    },
    indent: 20,
    features: [asyncDataLoaderFeature, selectionFeature, hotkeysCoreFeature],
  });

  const getNodeTypeInfo = useCallback(
    (nodeId: string) => {
      if (!(nodeId in graph.data_map)) return null;
      return classifyNode(nodeId, graph);
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [graphVersion],
  );

  const renderNodePreview = useCallback(
    (nodeId: string) => {
      if (!(nodeId in graph.data_map)) return null;
      const nodeData = graph.data_map[nodeId];
      const preview = getNodePreviewData(nodeData);
      const isDuplicate = duplicateNodes.has(nodeId);

      return (
        <PreviewPopover id={`preview-${nodeId}`}>
          <Popover.Body>
            <div className="preview-type">{preview.type}</div>
            {preview.fields.map(
              (f, i) =>
                f.value && (
                  <div key={i} className="preview-field">
                    <strong>{f.label}:</strong> {f.value}
                  </div>
                ),
            )}
            {renderTagPreview(preview.tags)}
            {isDuplicate && (
              <div className="preview-duplicate-warn">Appears in multiple locations in this graph (loop)</div>
            )}
          </Popover.Body>
        </PreviewPopover>
      );
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [graphVersion, duplicateNodes],
  );

  return (
    <Tree>
      <div {...tree.getContainerProps()} className="tree">
        {tree.getItems().map((item) => {
          const nodeId = item.getId();
          const typeInfo = getNodeTypeInfo(nodeId);
          const isDuplicate = duplicateNodes.has(nodeId);
          const isHighlighted = highlightedNodeId !== null && highlightedNodeId === nodeId;

          return (
            <OverlayTrigger
              key={nodeId}
              placement="right"
              delay={{ show: 400, hide: 100 }}
              overlay={renderNodePreview(nodeId) ?? <span />}
            >
              <button
                {...item.getProps()}
                style={{ paddingLeft: `${item.getItemMeta().level * 20}px` }}
                onMouseEnter={() => {
                  if (isDuplicate) setHighlightedNodeId(nodeId);
                }}
                onMouseLeave={() => {
                  if (highlightedNodeId === nodeId) setHighlightedNodeId(null);
                }}
                onClick={(e) => {
                  item.getProps().onClick?.(e);
                  if (isDuplicate) {
                    setHighlightedNodeId((prev) => (prev === nodeId ? null : nodeId));
                  }
                }}
              >
                <div
                  className={cn('treeitem pt-1 pb-0', {
                    focused: item.isFocused(),
                    expanded: item.isExpanded(),
                    selected: item.isSelected(),
                    folder: item.isFolder(),
                    'duplicate-highlight': isDuplicate && isHighlighted,
                  })}
                >
                  {typeInfo && (
                    <span
                      className="node-type-badge"
                      style={{ backgroundColor: NODE_TYPE_COLORS[typeInfo.nodeType] ?? NODE_TYPE_COLORS.other }}
                    >
                      {NODE_TYPE_LABELS[typeInfo.nodeType] ?? 'Other'}
                    </span>
                  )}
                  {item.getItemName()}
                  {isDuplicate && <span className="duplicate-indicator" title="Appears multiple times in graph">loop</span>}
                  {item.isLoading() && <Spinner className="m-4 loading" animation="border" />}
                </div>
              </button>
            </OverlayTrigger>
          );
        })}
      </div>
    </Tree>
  );
};

export const AssociationTree: React.FC = () => {
  return (
    <ErrorBoundary fallback={<RenderErrorAlert page={false} />}>
      <AssociationTreeComponent />
    </ErrorBoundary>
  );
};

export default AssociationTree;
