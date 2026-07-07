// spec: ./EntityBrowser.spec.md
import React, { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';

// project imports
import {
  defaultBidirectional,
  DOWN_DEFAULT_CFG,
  findMultiParentNodeIds,
  hasContextualDisplayChildren,
  TreeIndex,
  TreeOrientation,
} from '../treeHelpers';
import {
  collectGroupOptions,
  collectTagOptions,
  computeFlaggedNodes,
  filterTree,
  focusBreadcrumb,
  getDepthFromClauses,
  getEntityLayerConfigFromClauses,
  nodeTypeOf,
  resolveRoots,
} from './browserHelpers';
import { FilterCriteria, LayerPolicy, LayerPolicyMap, RootDescriptor, RootSpec, TraversalConfig } from './types';
import { useGraphData } from '../../data/GraphDataContext';
import { computeDistances } from '../../data/graphMerge';
import { getNodeName } from '../../utilities';
import { useSharedTreeIndex } from '../../data/SharedTreeIndex';
import { Clause } from '@components/shared/inputs/omnibar/ClauseTypes';
import { getSearchTextFromClauses, getStringFieldListFromClauses, getTagsFromClauses } from '@components/shared/inputs/omnibar/utils';
import { TagOptions } from '@models/tags';
import { NodeType } from '@models/trees';

interface EntityBrowserContextValue {
  // graph-derived (recomputed on graphVersion)
  index: TreeIndex;
  roots: RootDescriptor[];
  /**
   * The node the tree is currently re-rooted (focused) at, or `null` for the natural roots. When set, {@link
   * roots} is that single node and the view is measured relative to it (indent resets, auto-expand is
   * focus-relative, and the depth bound is lifted so the whole loaded subtree is browsable).
   */
  focusRoot: string | null;
  /** Re-root the tree at `id` (or `null` to restore the natural roots). */
  setFocusRoot: (id: string | null) => void;
  /** The focus breadcrumb top→down (incl. the focus root as the last entry); empty when not focused. */
  focusAncestors: RootDescriptor[];
  multiParent: Set<string>;
  presentKinds: NodeType[];
  tagOptions: TagOptions;
  groupOptions: string[];
  /** Node ids allowed by the active filter, or `null` when no filter is active (render everything). */
  visibleSet: Set<string> | null;
  /** Layer policies + depth bound governing traversal/rendering. */
  traversalConfig: TraversalConfig;
  // omnibar + flagged state
  clauses: Clause[];
  setClauses: (next: Clause[]) => void;
  flaggedOnly: boolean;
  setFlaggedOnly: (b: boolean) => void;
  // per-node hide (entities view only): hidden ids and their whole subtrees are dropped from the tree
  hiddenNodes: Set<string>;
  /** Hide a node (and its entire subtree) by id. */
  hideNode: (id: string) => void;
  /** Unhide a single previously-hidden node by id. */
  unhideNode: (id: string) => void;
  /** Clear all hidden nodes. */
  unhideAll: () => void;
  /** Human-readable label for a (possibly hidden) node id, for the hidden-nodes list. */
  labelForNode: (id: string) => string;
  // per-row expansion (keyed by a path-unique row key so DAG duplicates expand independently). A row is
  // expanded when explicitly expanded, OR auto-expanded because it's within the current depth (and not
  // explicitly collapsed) — so raising the depth reveals the nesting without a manual click per level.
  isChildrenExpanded: (rowKey: string, nodeId: string, viaReversed?: boolean, reverseDepth?: number) => boolean;
  /** True only when the user explicitly expanded this row (vs. depth-driven auto-expand). */
  isChildrenExplicit: (rowKey: string) => boolean;
  setChildrenExpanded: (rowKey: string, expanded: boolean) => void;
  /** Grow-once guard shared across rows (growth mutates the shared graph). */
  grownNodes: Set<string>;
}

const EntityBrowserContext = createContext<EntityBrowserContextValue | undefined>(undefined);

export const useEntityBrowser = (): EntityBrowserContextValue => {
  const ctx = useContext(EntityBrowserContext);
  if (ctx === undefined) {
    throw new Error('useEntityBrowser must be used within an EntityBrowserProvider');
  }
  return ctx;
};

interface EntityBrowserProviderProps {
  roots: RootSpec;
  defaultPolicies?: LayerPolicyMap;
  fallbackPolicy?: LayerPolicy;
  defaultDepth?: number;
  /**
   * Optional controlled omnibar clauses. When provided (with {@link setClauses}), the provider is controlled
   * and the caller owns the clause state (e.g. a URL-backed dashboard); when omitted it falls back to internal
   * `useState`. Standard controlled/uncontrolled pattern.
   */
  clauses?: Clause[];
  /** Setter for controlled {@link clauses}. Required for the clauses to be controlled. */
  setClauses?: (next: Clause[]) => void;
  /**
   * Optional controlled hidden-node set. When provided (with {@link onHiddenNodesChange}), the caller owns the
   * hidden set (e.g. the dashboard keeps it in the URL); when omitted it falls back to internal `useState`.
   */
  hiddenNodes?: Set<string>;
  /** Change handler for controlled {@link hiddenNodes}. Required for the set to be controlled. */
  onHiddenNodesChange?: (next: Set<string>) => void;
  /** Optional controlled flagged-only toggle; falls back to internal `useState` when omitted. */
  flaggedOnly?: boolean;
  /** Setter for controlled {@link flaggedOnly}. Required for the toggle to be controlled. */
  setFlaggedOnly?: (b: boolean) => void;
  /**
   * Optional controlled focus root (re-rooted subtree). When provided (with {@link onFocusRootChange}), the
   * caller owns it (e.g. the dashboard keeps it in the URL); when omitted it falls back to internal `useState`.
   */
  focusRoot?: string | null;
  /** Change handler for controlled {@link focusRoot}. Required for the focus root to be controlled. */
  onFocusRootChange?: (id: string | null) => void;
  children: React.ReactNode;
}

/**
 * Holds the entity browser's UI state (omnibar clauses, flagged toggle, per-row expansion) and the
 * graph-derived structures (index, roots, options, filter set, traversal config) so nested levels/rows read
 * them from context instead of prop-drilling. Layer policies, filters, and depth are all *derived from the
 * omnibar clauses*. Raising the depth clause additively grows the shared graph via {@link useGraphData}
 * (never a full refetch).
 */
export const EntityBrowserProvider: React.FC<EntityBrowserProviderProps> = ({
  roots: rootSpec,
  defaultPolicies = {},
  fallbackPolicy = LayerPolicy.Show,
  defaultDepth = 0,
  clauses: controlledClauses,
  setClauses: controlledSetClauses,
  hiddenNodes: controlledHiddenNodes,
  onHiddenNodesChange,
  flaggedOnly: controlledFlaggedOnly,
  setFlaggedOnly: controlledSetFlaggedOnly,
  focusRoot: controlledFocusRoot,
  onFocusRootChange,
  children,
}) => {
  const { graph, graphId, graphVersion, growToDepth, growable } = useGraphData();

  // clauses / flagged / hidden each follow the standard controlled-or-uncontrolled pattern: when the caller
  // supplies the value + setter the provider is controlled, otherwise it manages the state internally
  const [internalClauses, setInternalClauses] = useState<Clause[]>([]);
  const clauses = controlledClauses ?? internalClauses;
  const setClauses = controlledSetClauses ?? setInternalClauses;
  const [internalFlaggedOnly, setInternalFlaggedOnly] = useState(false);
  const flaggedOnly = controlledFlaggedOnly ?? internalFlaggedOnly;
  const setFlaggedOnly = controlledSetFlaggedOnly ?? setInternalFlaggedOnly;
  const [internalHiddenNodes, setInternalHiddenNodes] = useState<Set<string>>(new Set());
  const hiddenNodes = controlledHiddenNodes ?? internalHiddenNodes;
  // apply a hidden-set update to whichever store owns it (controlled callback or internal state)
  const applyHiddenNodes = useCallback(
    (updater: (prev: Set<string>) => Set<string>) => {
      if (onHiddenNodesChange) {
        onHiddenNodesChange(updater(controlledHiddenNodes ?? new Set()));
      } else {
        setInternalHiddenNodes((prev) => updater(prev));
      }
    },
    [onHiddenNodesChange, controlledHiddenNodes],
  );
  const hideNode = useCallback(
    (id: string) => {
      applyHiddenNodes((prev) => {
        const next = new Set(prev);
        next.add(id);
        return next;
      });
    },
    [applyHiddenNodes],
  );
  const unhideNode = useCallback(
    (id: string) => {
      applyHiddenNodes((prev) => {
        const next = new Set(prev);
        next.delete(id);
        return next;
      });
    },
    [applyHiddenNodes],
  );
  const unhideAll = useCallback(() => {
    applyHiddenNodes(() => new Set());
  }, [applyHiddenNodes]);
  // focus root follows the same controlled-or-uncontrolled pattern: re-root the tree at a node (indent resets,
  // view measured relative to it) or clear it to restore the natural roots
  const [internalFocusRoot, setInternalFocusRoot] = useState<string | null>(null);
  const focusRoot = controlledFocusRoot !== undefined ? controlledFocusRoot : internalFocusRoot;
  const setFocusRoot = useCallback(
    (id: string | null) => {
      if (onFocusRootChange) onFocusRootChange(id);
      else setInternalFocusRoot(id);
    },
    [onFocusRootChange],
  );
  // explicit user expands / collapses layered over the depth-driven auto-expand default
  const [expandedChildren, setExpandedChildren] = useState<Set<string>>(new Set());
  const [collapsedChildren, setCollapsedChildren] = useState<Set<string>>(new Set());
  const grownNodesRef = useRef<Set<string>>(new Set());

  const setChildrenExpanded = useCallback((rowKey: string, expanded: boolean) => {
    setExpandedChildren((prev) => {
      const next = new Set(prev);
      if (expanded) next.add(rowKey);
      else next.delete(rowKey);
      return next;
    });
    setCollapsedChildren((prev) => {
      const next = new Set(prev);
      if (expanded) next.delete(rowKey);
      else next.add(rowKey);
      return next;
    });
  }, []);

  // graph-derived structures; recomputed only when the shared graph version bumps
  // the tree index is derived ONCE in the shared layer and consumed here (and by the association-tree overlay)
  const { index } = useSharedTreeIndex();
  // structural key for the root spec so inline `{ kind: 'sha256', sha256 }` literals (a fresh object every
  // parent render) don't re-run the O(data_map) resolveRoots scan and churn the roots array identity
  const rootSpecKey = useMemo(() => {
    switch (rootSpec.kind) {
      case 'sha256':
        return `sha256:${rootSpec.sha256}`;
      case 'nodes':
        return `nodes:${rootSpec.roots.map((r) => r.id).join(',')}`;
      case 'initial':
        return 'initial';
    }
  }, [rootSpec]);
  // resolveRoots depends only on the spec's structural content (captured by rootSpecKey), the graph, and the
  // index; keying on rootSpecKey instead of the spec object avoids recomputes from inline-literal identity.
  // When a focus root is active (and loaded), the tree is re-rooted at that single node instead.
  const roots = useMemo(() => {
    if (focusRoot && focusRoot in graph.data_map) {
      const node = graph.data_map[focusRoot];
      return [{ id: focusRoot, label: (node ? getNodeName(node, 80) : '') || focusRoot }];
    }
    return resolveRoots(graph, rootSpec, index);
  }, [graphVersion, rootSpecKey, index, focusRoot]);
  // the breadcrumb trail back up from a focus root (top→down, incl. the focus root); empty when not focused
  const focusAncestors = useMemo(
    () => (focusRoot && focusRoot in graph.data_map ? focusBreadcrumb(graph, index, focusRoot) : []),
    [graphVersion, index, focusRoot],
  );
  const multiParent = useMemo(() => findMultiParentNodeIds(graph, index), [graphVersion, index]);
  const distances = useMemo(() => computeDistances(graph), [graphVersion]);
  // when focused, measure auto-expand depth relative to the focus root so a re-rooted subtree expands its own
  // first levels (distances from the far-away original seeds would otherwise leave it collapsed)
  const focusDistances = useMemo(
    () => (focusRoot && focusRoot in graph.data_map ? computeDistances(graph, [focusRoot]) : null),
    [graphVersion, focusRoot],
  );
  const effectiveDistances = focusDistances ?? distances;
  const flaggedNodes = useMemo(() => computeFlaggedNodes(graph, index), [graphVersion, index]);
  const tagOptions = useMemo(() => collectTagOptions(graph), [graphVersion]);
  const groupOptions = useMemo(() => collectGroupOptions(graph), [graphVersion]);
  const presentKinds = useMemo(() => {
    const kinds = new Set<NodeType>();
    for (const nodeId of Object.keys(graph.data_map ?? {})) {
      kinds.add(nodeTypeOf(nodeId, graph));
    }
    return Array.from(kinds);
  }, [graphVersion]);

  // clause-derived filter/policy inputs
  const layerConfig = useMemo(() => getEntityLayerConfigFromClauses(clauses), [clauses]);
  // 0 => no explicit depth clause => no depth bound (show everything pulled)
  const depthClause = useMemo(() => getDepthFromClauses(clauses, 0), [clauses]);
  const maxDepth = depthClause > 0 ? depthClause : null;
  // focus is only "applied" once its node is actually loaded — a stale URL focus for an absent node falls back
  // to the natural roots (see `roots`), so it must NOT also lift the depth bound / re-base distances
  const focusApplied = focusDistances !== null;
  // while focused the depth bound is lifted so the whole loaded subtree under the focus root is browsable
  // (the bound is distance-from-original-seeds, which would otherwise prune the re-rooted subtree)
  const effectiveMaxDepth = focusApplied ? null : maxDepth;
  // rows within this many hops of the seeds (or the focus root, when focused) auto-expand so the loaded
  // nesting shows. The explicit depth clause takes precedence; otherwise the component's `defaultDepth`.
  const autoExpandDepth = maxDepth ?? defaultDepth;
  const isChildrenExpanded = useCallback(
    (rowKey: string, nodeId: string, viaReversed = false, reverseDepth = 0) => {
      if (collapsedChildren.has(rowKey)) return false;
      if (expandedChildren.has(rowKey)) return true;
      // a growable node with NO loaded children yet stays collapsed under auto-expand (nothing to show — the
      // grow affordance stands in for it). A growable node that ALREADY has loaded children still auto-expands,
      // otherwise its loaded descendants (e.g. a SigmaRule's Flag children) would be hidden while it remains
      // growable. The grow badge continues to signal more can be loaded. The child presence check honors the
      // row's arrival context so a reverse-reached node isn't judged by its (suppressed) forward children.
      if (growable.has(nodeId) && !hasContextualDisplayChildren(index, nodeId, DOWN_DEFAULT_CFG, viaReversed, reverseDepth))
        return false;
      return autoExpandDepth > 0 && (effectiveDistances.get(nodeId) ?? Infinity) < autoExpandDepth;
    },
    [collapsedChildren, expandedChildren, growable, effectiveDistances, autoExpandDepth, index],
  );
  const isChildrenExplicit = useCallback((rowKey: string) => expandedChildren.has(rowKey), [expandedChildren]);
  // resolve a display label for a (possibly hidden) node id so the hidden-nodes control can list what's hidden
  const labelForNode = useCallback(
    (id: string) => {
      const node = graph.data_map?.[id];
      return (node ? getNodeName(node, 80) : '') || id;
    },
    [graph],
  );
  const text = useMemo(() => getSearchTextFromClauses(clauses), [clauses]);
  const tags = useMemo(() => getTagsFromClauses(clauses), [clauses]);
  const groups = useMemo(() => getStringFieldListFromClauses(clauses, 'group'), [clauses]);

  const traversalConfig = useMemo<TraversalConfig>(
    () => ({
      clausePolicies: layerConfig.policies,
      includeSet: layerConfig.includeSet,
      defaultPolicies,
      fallback: fallbackPolicy,
      maxDepth: effectiveMaxDepth,
      distances: effectiveDistances,
      hiddenNodes,
      // the entity browser surfaces relationship (non-structural) associations against their stored direction
      // so e.g. a WindowsProcess shows its Flags and each Flag its SigmaRule; containment stays directional
      orientation: TreeOrientation.Down,
      bidirectional: defaultBidirectional,
    }),
    [layerConfig, defaultPolicies, fallbackPolicy, effectiveMaxDepth, effectiveDistances, hiddenNodes],
  );

  const criteria = useMemo<FilterCriteria>(
    () => ({ text, tags, groups, flaggedOnly, flaggedNodes }),
    [text, tags, groups, flaggedOnly, flaggedNodes],
  );

  // depth bounding is applied in effectiveChildren (via traversalConfig), so the visible-set filter only
  // needs to reflect the client-side match criteria (text / tags / groups / flagged)
  const filterActive = text.trim().length > 0 || Object.keys(tags).length > 0 || groups.length > 0 || flaggedOnly;
  const visibleSet = useMemo(
    () =>
      filterActive
        ? filterTree(
            roots.map((r) => r.id),
            index,
            graph,
            criteria,
            traversalConfig,
          )
        : null,
    [graphVersion, index, roots, criteria, traversalConfig, filterActive],
  );

  // grow the shared graph to the deepest requested level (the explicit depth clause OR the component's initial
  // `defaultDepth`). The provider's `growToDepth` owns the raise-only, tree-scoped, success-gated guard, so a
  // redundant/lower request (or one racing the 3D graph's own trigger) is a no-op there — this effect just
  // forwards the target. Gated on `graphId` so we don't fire before the initial fetch lands.
  const growTarget = Math.max(maxDepth ?? 0, defaultDepth);
  useEffect(() => {
    if (!graphId || growTarget <= 1) return;
    void growToDepth(growTarget);
  }, [graphId, growTarget, growToDepth]);

  const value = useMemo<EntityBrowserContextValue>(
    () => ({
      index,
      roots,
      focusRoot,
      setFocusRoot,
      focusAncestors,
      multiParent,
      presentKinds,
      tagOptions,
      groupOptions,
      visibleSet,
      traversalConfig,
      clauses,
      setClauses,
      flaggedOnly,
      setFlaggedOnly,
      hiddenNodes,
      hideNode,
      unhideNode,
      unhideAll,
      labelForNode,
      isChildrenExpanded,
      isChildrenExplicit,
      setChildrenExpanded,
      grownNodes: grownNodesRef.current,
    }),
    [
      index,
      roots,
      focusRoot,
      setFocusRoot,
      focusAncestors,
      multiParent,
      presentKinds,
      tagOptions,
      groupOptions,
      visibleSet,
      traversalConfig,
      clauses,
      setClauses,
      flaggedOnly,
      setFlaggedOnly,
      hiddenNodes,
      hideNode,
      unhideNode,
      unhideAll,
      labelForNode,
      isChildrenExpanded,
      isChildrenExplicit,
      setChildrenExpanded,
    ],
  );

  return <EntityBrowserContext.Provider value={value}>{children}</EntityBrowserContext.Provider>;
};
