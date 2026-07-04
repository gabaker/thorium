// spec: ./EntityBrowser.spec.md
import React, { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';

// project imports
import { buildTreeIndex, findMultiParentNodeIds, TreeIndex } from '../treeHelpers';
import {
  collectGroupOptions,
  collectTagOptions,
  computeFlaggedNodes,
  filterTree,
  getDepthFromClauses,
  getEntityLayerConfigFromClauses,
  nodeTypeOf,
  resolveRoots,
} from './browserHelpers';
import { FilterCriteria, LayerPolicy, LayerPolicyMap, RootDescriptor, RootSpec, TraversalConfig } from './types';
import { useGraphData } from '../../data/GraphDataContext';
import { computeDistances } from '../../data/graphMerge';
import { Clause } from '@components/shared/inputs/omnibar/ClauseTypes';
import { getSearchTextFromClauses, getStringFieldListFromClauses, getTagsFromClauses } from '@components/shared/inputs/omnibar/utils';
import { TagOptions } from '@models/tags';
import { NodeType } from '@models/trees';

interface EntityBrowserContextValue {
  // graph-derived (recomputed on graphVersion)
  index: TreeIndex;
  roots: RootDescriptor[];
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
  // per-row expansion (keyed by a path-unique row key so DAG duplicates expand independently). A row is
  // expanded when explicitly expanded, OR auto-expanded because it's within the current depth (and not
  // explicitly collapsed) — so raising the depth reveals the nesting without a manual click per level.
  isChildrenExpanded: (rowKey: string, nodeId: string) => boolean;
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
  children,
}) => {
  const { graph, graphId, graphVersion, growToDepth, growable } = useGraphData();

  const [clauses, setClauses] = useState<Clause[]>([]);
  const [flaggedOnly, setFlaggedOnly] = useState(false);
  // explicit user expands / collapses layered over the depth-driven auto-expand default
  const [expandedChildren, setExpandedChildren] = useState<Set<string>>(new Set());
  const [collapsedChildren, setCollapsedChildren] = useState<Set<string>>(new Set());
  const grownNodesRef = useRef<Set<string>>(new Set());
  // largest depth we've already grown to, so raising the depth clause grows additively and only once
  const maxRequestedDepthRef = useRef(0);
  // FIX (cross-tree race): the grown depth is per-tree; track which tree it applies to so a swap to a new
  // tree id (StrictMode/remount) resets the guard and the new tree gets grown instead of being skipped.
  const grownForGraphIdRef = useRef<string>('');

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
  const index = useMemo(() => buildTreeIndex(graph), [graphVersion]);
  const roots = useMemo(() => resolveRoots(graph, rootSpec, index), [graphVersion, rootSpec, index]);
  const multiParent = useMemo(() => findMultiParentNodeIds(graph, index), [graphVersion, index]);
  const distances = useMemo(() => computeDistances(graph), [graphVersion]);
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
  // rows within this many hops of the seeds auto-expand (so the loaded nesting shows). The explicit depth
  // clause takes precedence; otherwise the component's `defaultDepth` (0 = nothing auto-expands).
  const autoExpandDepth = maxDepth ?? defaultDepth;
  const isChildrenExpanded = useCallback(
    (rowKey: string, nodeId: string) => {
      if (collapsedChildren.has(rowKey)) return false;
      if (expandedChildren.has(rowKey)) return true;
      // a still-growable node stays collapsed under auto-expand — its children aren't fully loaded yet, so
      // showing it "expanded" alongside the grow affordance would be contradictory; the user grows it by
      // explicitly expanding (which lands in `expandedChildren` above).
      if (growable.has(nodeId)) return false;
      return autoExpandDepth > 0 && (distances.get(nodeId) ?? Infinity) < autoExpandDepth;
    },
    [collapsedChildren, expandedChildren, growable, distances, autoExpandDepth],
  );
  const isChildrenExplicit = useCallback((rowKey: string) => expandedChildren.has(rowKey), [expandedChildren]);
  const text = useMemo(() => getSearchTextFromClauses(clauses), [clauses]);
  const tags = useMemo(() => getTagsFromClauses(clauses), [clauses]);
  const groups = useMemo(() => getStringFieldListFromClauses(clauses, 'group'), [clauses]);

  const traversalConfig = useMemo<TraversalConfig>(
    () => ({
      clausePolicies: layerConfig.policies,
      includeSet: layerConfig.includeSet,
      defaultPolicies,
      fallback: fallbackPolicy,
      maxDepth,
      distances,
    }),
    [layerConfig, defaultPolicies, fallbackPolicy, maxDepth, distances],
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

  // raise-only: grow the shared graph to the deepest requested level (the explicit depth clause OR the
  // component's initial `defaultDepth`), additively (mergeGrowthInto) — never reload(). Gated on `graphId`
  // so we don't mark the depth as "requested" before the initial fetch lands (which would otherwise swallow
  // the grow). Mirrors the association graph's own depth-increase effect.
  const growTarget = Math.max(maxDepth ?? 0, defaultDepth);
  useEffect(() => {
    if (!graphId) return;
    // FIX (cross-tree race): reset the per-tree depth guard when the shared graph swaps to a new tree, so the
    // final tree is grown to the requested depth instead of being skipped by a guard set on an abandoned tree.
    if (grownForGraphIdRef.current !== graphId) {
      // DEBUG (remove after diagnosing entities-tab depth 400s)
      console.warn('[eb-debug] graphId changed — resetting depth grow guard', { from: grownForGraphIdRef.current, to: graphId });
      grownForGraphIdRef.current = graphId;
      maxRequestedDepthRef.current = 0;
    }
    if (growTarget > 1 && growTarget > maxRequestedDepthRef.current) {
      // DEBUG (remove after diagnosing entities-tab depth 400s): the entities tab's auto grow-to-depth
      // trigger (fires on mount/remount and when the depth clause increases). The graph never auto-grows.
      console.warn('[eb-debug] EntityBrowser growToDepth trigger', { graphId, growTarget, prevMax: maxRequestedDepthRef.current });
      maxRequestedDepthRef.current = growTarget;
      void growToDepth(growTarget);
    }
  }, [graphId, growTarget, growToDepth]);

  const value = useMemo<EntityBrowserContextValue>(
    () => ({
      index,
      roots,
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
      isChildrenExpanded,
      isChildrenExplicit,
      setChildrenExpanded,
      grownNodes: grownNodesRef.current,
    }),
    [
      index,
      roots,
      multiParent,
      presentKinds,
      tagOptions,
      groupOptions,
      visibleSet,
      traversalConfig,
      clauses,
      flaggedOnly,
      isChildrenExpanded,
      isChildrenExplicit,
      setChildrenExpanded,
    ],
  );

  return <EntityBrowserContext.Provider value={value}>{children}</EntityBrowserContext.Provider>;
};
