import React, { useState, useMemo, useCallback, useEffect, useRef } from 'react';
import { asyncDataLoaderFeature, hotkeysCoreFeature, selectionFeature } from '@headless-tree/core';
import { useTree } from '@headless-tree/react';
import cn from 'classnames';
import { OverlayTrigger, Popover, Spinner } from 'react-bootstrap';
import { ErrorBoundary } from 'react-error-boundary';

import RenderErrorAlert from '../../shared/alerts/RenderErrorAlert';
import { BranchNode } from '@models/trees';
import { getNodeName } from '../utilities';
import { classifyNode } from '../graph-d3/data';
import { getNodeSvg } from '../graph-d3/styles';
import { useGraphData } from '../data';
import { TreeContainer, PreviewPopover } from './AssociationTree.styled';
import { NODE_TYPE_LABELS, getNodePreviewData, renderTagPreview, findMultiParentNodeIds } from './treeHelpers';

const AssociationTreeComponent: React.FC = () => {
  const { graph, graphVersion, grow, growable, getGraph, focusedNodeId, focusSource, setFocusedNode } = useGraphData();
  const [loadingItemData, setLoadingItemData] = useState<string[]>([]);
  const [loadingItemChildrens, setLoadingItemChildrens] = useState<string[]>([]);
  const [highlightedNodeId, setHighlightedNodeId] = useState<string | null>(null);

  const multiParentNodes = useMemo(() => {
    if (!graph.id) return new Set<string>();
    return findMultiParentNodeIds(graph);
    // graphVersion drives recomputation when the underlying ref changes
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graphVersion]);

  const tree = useTree<string>({
    state: { loadingItemData, loadingItemChildrens },
    setLoadingItemData,
    setLoadingItemChildrens,
    rootItemId: 'root',
    getItemName: (node) => {
      const nodeId = node.getId();
      const g = getGraph();
      if (g.data_map && nodeId in g.data_map) {
        return getNodeName(g.data_map[nodeId], 100);
      }
      return node.getItemData();
    },
    isItemFolder: (node) => {
      const nodeId = node.getId();
      const g = getGraph();
      const branches = g.branches ? Object.keys(g.branches) : [];
      return !!(g.growable?.includes(nodeId) || branches.includes(nodeId));
    },
    createLoadingItemData: () => 'loading...',
    dataLoader: {
      getItem: (nodeId) => nodeId,
      getChildren: async (nodeId) => {
        if (nodeId == 'root') {
          return getGraph().initial;
        }

        if (growable.has(nodeId)) {
          await grow(nodeId);
        }

        const latest = getGraph();
        const children: string[] = [];
        if (latest.branches && nodeId in latest.branches) {
          latest.branches[nodeId].forEach((node: BranchNode) => children.push(node.node));
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
      const isDuplicate = multiParentNodes.has(nodeId);

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
              <div className="preview-duplicate-warn">Duplicate: appears under multiple parents in this tree</div>
            )}
          </Popover.Body>
        </PreviewPopover>
      );
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [graphVersion, multiParentNodes],
  );

  const pendingFocusRef = useRef<string | null>(null);

  // When graph clicks a node, expand ancestors in tree, select it, and scroll to it
  useEffect(() => {
    if (!focusedNodeId || focusSource !== 'graph') return;

    const expandAndSelect = async () => {
      const g = getGraph();
      if (!g.branches) return;

      // Build child→parent map
      const parentMap = new Map<string, string>();
      for (const [parent, children] of Object.entries(g.branches)) {
        for (const child of children) {
          parentMap.set(child.node, parent);
        }
      }

      // Walk up to build ancestor chain (root-first)
      const ancestors: string[] = [];
      let current = focusedNodeId;
      while (parentMap.has(current)) {
        current = parentMap.get(current)!;
        ancestors.unshift(current);
      }

      // Expand each ancestor sequentially, loading children as needed
      for (const ancestorId of ancestors) {
        try {
          const item = tree.getItemInstance(ancestorId);
          if (item.isFolder() && !item.isExpanded()) {
            item.expand();
            await tree.loadChildrenIds(ancestorId);
          }
        } catch {
          pendingFocusRef.current = focusedNodeId;
          return;
        }
      }

      // Small delay for tree to rebuild after expansions
      await new Promise((r) => setTimeout(r, 50));

      try {
        const targetItem = tree.getItemInstance(focusedNodeId);
        targetItem.select();
        targetItem.setFocused();
        void targetItem.scrollTo({ block: 'nearest' });
      } catch {
        // Node not yet visible in tree — will sync when tree rebuilds
      }
    };

    void expandAndSelect();
  }, [focusedNodeId, focusSource]);

  // Retry pending focus after graph version changes (tree data updated)
  useEffect(() => {
    if (!pendingFocusRef.current) return;
    const pending = pendingFocusRef.current;
    pendingFocusRef.current = null;

    const retryFocus = async () => {
      await new Promise((r) => setTimeout(r, 100));
      try {
        const item = tree.getItemInstance(pending);
        item.select();
        item.setFocused();
        void item.scrollTo({ block: 'nearest' });
      } catch {
        // Still not available
      }
    };

    void retryFocus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graphVersion]);

  return (
    <TreeContainer>
      <div {...tree.getContainerProps()} className="tree">
        {tree.getItems().map((item) => {
          const nodeId = item.getId();
          const typeInfo = getNodeTypeInfo(nodeId);
          const isDuplicate = multiParentNodes.has(nodeId);
          const isHighlighted = highlightedNodeId !== null && highlightedNodeId === nodeId;

          return (
            <OverlayTrigger
              key={nodeId}
              placement="right-start"
              delay={{ show: 400, hide: 100 }}
              overlay={renderNodePreview(nodeId) ?? <span />}
              popperConfig={{ modifiers: [{ name: 'offset', options: { offset: [0, 8] } }] }}
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
                  setFocusedNode(nodeId, 'tree');
                  if (isDuplicate) {
                    setHighlightedNodeId((prev) => (prev === nodeId ? null : nodeId));
                  }
                }}
              >
                <div
                  className={cn('treeitem', {
                    focused: item.isFocused(),
                    expanded: item.isExpanded(),
                    selected: item.isSelected(),
                    folder: item.isFolder(),
                    'duplicate-highlight': isDuplicate && isHighlighted,
                  })}
                >
                  {typeInfo && (
                    <span
                      className="node-type-icon"
                      title={NODE_TYPE_LABELS[typeInfo.nodeType] ?? 'Other'}
                      dangerouslySetInnerHTML={{ __html: getNodeSvg(typeInfo.nodeType, typeInfo.visualState) }}
                    />
                  )}
                  {item.getItemName()}
                  {isDuplicate && <span className="duplicate-indicator" title="Duplicate: has multiple parents in graph">Duplicate</span>}
                  {item.isLoading() && <Spinner animation="border" size="sm" className="loading" style={{ width: 14, height: 14, marginLeft: 6, borderWidth: 2 }} />}
                </div>
              </button>
            </OverlayTrigger>
          );
        })}
      </div>
    </TreeContainer>
  );
};

export const AssociationTree: React.FC = () => {
  const { graph } = useGraphData();
  return (
    <ErrorBoundary fallback={<RenderErrorAlert page={false} />}>
      {graph.id ? <AssociationTreeComponent key={graph.id} /> : <Spinner animation="border" />}
    </ErrorBoundary>
  );
};

export default AssociationTree;
