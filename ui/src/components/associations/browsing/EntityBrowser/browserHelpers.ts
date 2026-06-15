// spec: ./EntityBrowser.spec.md

// project imports
import { buildTreeIndex, contextualDisplayEdges, parentIdsOf, toDisplayCfg, TreeIndex } from '../treeHelpers';
import { classifyNode } from '../../graph/data';
import { getNodeName } from '../../utilities';
import {
  EffectiveChild,
  EntityLayerConfig,
  FilterCriteria,
  KindGroup,
  LayerPolicy,
  RootDescriptor,
  RootSpec,
  TraversalConfig,
} from './types';
import { flatTagsToTags } from '@components/shared/info/info';
import { Clause } from '@components/shared/inputs/omnibar/ClauseTypes';
import { getStringFieldListFromClauses } from '@components/shared/inputs/omnibar/utils';
import { DangerTagKeys } from '@components/tags/tag_groups';
import { filterIncludedTags } from '@components/tags/utilities';
import { Entities } from '@models/entities';
import { RequestTags, TagOptions, Tags } from '@models/tags';
import { Graph, NodeType, TreeNode, TreeNodeKey } from '@models/trees';

/** Deduplicate a string list preserving order. */
function uniq(values: string[]): string[] {
  return Array.from(new Set(values));
}

/**
 * Find the graph node id for the file with the given sha256, if present.
 *
 * @param graph - The shared association graph.
 * @param sha256 - The file sha256 to locate.
 * @returns The node id, or `undefined` when the file isn't in the graph.
 */
export function findFileNodeHash(graph: Graph, sha256: string): string | undefined {
  for (const [nodeId, node] of Object.entries(graph.data_map ?? {})) {
    if (node[TreeNodeKey.Sample]?.sha256 === sha256) {
      return nodeId;
    }
  }
  return undefined;
}

/** The display label for a node id (its name), falling back to the raw id. */
function labelFor(graph: Graph, nodeId: string): string {
  const node = graph.data_map[nodeId];
  return node ? getNodeName(node, 100) || nodeId : nodeId;
}

/** The classified {@link NodeType} of a node id (guards nodes missing from `data_map`). */
export function nodeTypeOf(nodeId: string, graph: Graph): NodeType {
  if (!(nodeId in graph.data_map)) return NodeType.Other;
  return classifyNode(nodeId, graph).nodeType;
}

/**
 * Resolve a node type's effective {@link LayerPolicy}. Precedence: explicit `Show`/`Hide`/`Exclude` clause →
 * `Include` whitelist membership (Show) → component default → `Include` present but not listed (PassThrough,
 * so a whitelist still surfaces included types nested under non-included ones) → fallback.
 */
export function resolvePolicy(nodeType: NodeType, cfg: TraversalConfig): LayerPolicy {
  const explicit = cfg.clausePolicies[nodeType];
  if (explicit !== undefined) return explicit;
  if (cfg.includeSet?.has(nodeType)) return LayerPolicy.Show;
  const dflt = cfg.defaultPolicies[nodeType];
  if (dflt !== undefined) return dflt;
  if (cfg.includeSet) return LayerPolicy.PassThrough;
  return cfg.fallback;
}

/** The nested {@link Tags} carried by any node kind (flat Tag-node tags are normalized). */
export function getNodeTags(node: TreeNode): Tags {
  if (node[TreeNodeKey.Sample]) return node[TreeNodeKey.Sample].tags ?? {};
  if (node[TreeNodeKey.Repo]) return node[TreeNodeKey.Repo].tags ?? {};
  if (node[TreeNodeKey.Entity]) return node[TreeNodeKey.Entity].tags ?? {};
  if (node[TreeNodeKey.Tag]) return flatTagsToTags(node[TreeNodeKey.Tag].tags);
  return {};
}

/** The groups a node belongs to (Entity `.groups`; Sample/Repo from their submissions). */
export function nodeGroups(node: TreeNode): string[] {
  if (node[TreeNodeKey.Entity]) return node[TreeNodeKey.Entity].groups ?? [];
  if (node[TreeNodeKey.Sample]) return uniq((node[TreeNodeKey.Sample].submissions ?? []).flatMap((s) => s.groups ?? []));
  if (node[TreeNodeKey.Repo]) return uniq((node[TreeNodeKey.Repo].submissions ?? []).flatMap((s) => s.groups ?? []));
  return [];
}

/** True when a node carries any danger-classified tag. */
export function hasDangerTags(tags: Tags): boolean {
  return Object.keys(filterIncludedTags(tags, DangerTagKeys)).length > 0;
}

/** The first parent of a node in the index, or null. */
function firstParent(index: TreeIndex, nodeId: string): string | null {
  return parentIdsOf(index, nodeId)[0] ?? null;
}

/**
 * Resolve a {@link RootSpec} into concrete root descriptors.
 *
 * `sha256` locates the file node; `nodes` is passed through; `initial` ascends each seed node to its tree
 * root (mirroring the association tree's root resolution) so a dashboard view starts from the top.
 *
 * @param graph - The shared graph.
 * @param spec - How to determine roots.
 * @param index - Optional prebuilt index (used for the `initial` ascent; built on demand otherwise).
 * @returns The resolved roots (id + label), possibly empty.
 */
export function resolveRoots(graph: Graph, spec: RootSpec, index?: TreeIndex): RootDescriptor[] {
  switch (spec.kind) {
    case 'sha256': {
      const id = findFileNodeHash(graph, spec.sha256);
      return id ? [{ id, label: labelFor(graph, id) }] : [];
    }
    case 'nodes':
      return spec.roots;
    case 'initial': {
      const idx = index ?? buildTreeIndex(graph);
      const roots: string[] = [];
      // Set alongside the array so the "already collected this root" check is O(1) rather than a linear scan
      // per seed (the ascent can converge many seeds onto the same tree root)
      const rootSet = new Set<string>();
      for (const initialId of graph.initial) {
        let current = initialId.toString();
        const visited = new Set<string>([current]);
        let parent = firstParent(idx, current);
        while (parent && !visited.has(parent)) {
          visited.add(parent);
          current = parent;
          parent = firstParent(idx, current);
        }
        if (!rootSet.has(current)) {
          rootSet.add(current);
          roots.push(current);
        }
      }
      return roots.map((id) => ({ id, label: labelFor(graph, id) }));
    }
  }
}

/** Whether a node is within the config's depth bound (null bound = unbounded). Shared by effectiveChildren/filterTree. */
function withinDepth(id: string, cfg: TraversalConfig): boolean {
  return cfg.maxDepth == null || (cfg.distances.get(id) ?? 0) <= cfg.maxDepth;
}

/**
 * Compute the children to render under a parent, applying each child's resolved {@link LayerPolicy} and the
 * depth bound.
 *
 * - Beyond-`maxDepth` and `Skip` children are pruned (not rendered, not explored).
 * - `PassThrough` children are elided: we traverse *through* them and graft their qualifying descendants onto
 *   this parent, recording a breadcrumb of the elided node names so the row still explains its origin.
 * - `Show` children are kept.
 *
 * A **per-path** visited set guards cycles while still allowing a node reachable via two distinct paths
 * (DAG re-convergence) to render under each. Grafted `Show` results are deduped by id within this call.
 *
 * @param parentId - The node whose children to resolve.
 * @param index - The edge-carrying tree index.
 * @param graph - The shared graph (for node types + labels).
 * @param cfg - Layer policies + depth bound.
 * @param path - Node ids already on the rendered path to `parentId` (inclusive) — for cycle guarding.
 * @param reverseDepth - How many reversed hops preceded `parentId`.
 * @param viaReversed - Whether `parentId` was itself reached via a reversed edge.
 * @returns The effective children, each with its edge and any pass-through breadcrumb.
 */
export function effectiveChildren(
  parentId: string,
  index: TreeIndex,
  graph: Graph,
  cfg: TraversalConfig,
  path: Set<string>,
  reverseDepth = 0,
  viaReversed = false,
): EffectiveChild[] {
  const out: EffectiveChild[] = [];
  const seen = new Set<string>();
  const displayCfg = toDisplayCfg(cfg);

  // `nodeViaReversed`/`nodeReverseDepth` are the ARRIVAL context of `nodeId`; contextualDisplayEdges applies
  // the reverse-traversal rules (suppress forward fan-out of a reverse-reached node; bound reverse hops), and
  // each surviving edge yields the CHILD's context, carried on the EffectiveChild so the row expands correctly.
  const walk = (nodeId: string, currentPath: Set<string>, breadcrumb: string[], nodeViaReversed: boolean, nodeReverseDepth: number) => {
    const edges = contextualDisplayEdges(index, nodeId, displayCfg, nodeViaReversed, nodeReverseDepth);
    for (const edge of edges) {
      const childId = edge.id;
      if (currentPath.has(childId)) continue; // cycle guard, scoped to this path
      // user-hidden nodes vanish with their whole subtree BEFORE the policy check, so hiding a PassThrough
      // node also suppresses the descendants it would otherwise graft up (not just the node itself)
      if (cfg.hiddenNodes?.has(childId)) continue;
      if (!withinDepth(childId, cfg)) continue; // depth bound (prunes deeper nodes, incl. through pass-through)
      const isReverse = !!edge.reversed;
      const childViaReversed = isReverse ? true : nodeViaReversed;
      const childReverseDepth = isReverse ? nodeReverseDepth + 1 : nodeReverseDepth;
      const policy = resolvePolicy(nodeTypeOf(childId, graph), cfg);
      if (policy === LayerPolicy.Skip) continue;
      if (policy === LayerPolicy.PassThrough) {
        const nextPath = new Set(currentPath);
        nextPath.add(childId);
        const via = labelFor(graph, childId) || edge.label;
        walk(childId, nextPath, [...breadcrumb, via], childViaReversed, childReverseDepth);
        continue;
      }
      // Show
      if (seen.has(childId)) continue;
      seen.add(childId);
      out.push({ edge, viaReversed: childViaReversed, reverseDepth: childReverseDepth, ...(breadcrumb.length ? { breadcrumb } : {}) });
    }
  };

  walk(parentId, path, [], viaReversed, reverseDepth);
  return out;
}

/**
 * Group sibling children by their {@link NodeType}, preserving first-appearance order.
 *
 * @param children - Effective children of a single parent.
 * @param graph - The shared graph (for node classification).
 * @returns Kind groups in the order their kinds first appear.
 */
export function groupByKind(children: EffectiveChild[], graph: Graph): KindGroup[] {
  const groups: KindGroup[] = [];
  const byType = new Map<NodeType, KindGroup>();
  for (const child of children) {
    const nodeType = nodeTypeOf(child.edge.id, graph);
    let group = byType.get(nodeType);
    if (!group) {
      group = { nodeType, children: [] };
      byType.set(nodeType, group);
      groups.push(group);
    }
    group.children.push(child);
  }
  return groups;
}

/** True when a node's tags satisfy every tag filter (each key present with an any-of value; case-insensitive). */
function matchesTags(nodeTags: Tags, filter: RequestTags): boolean {
  return Object.entries(filter).every(([key, values]) => {
    const nodeKey = Object.keys(nodeTags).find((k) => k.toLowerCase() === key.toLowerCase());
    if (!nodeKey) return false;
    if (values.length === 0) return true;
    const nodeVals = Object.keys(nodeTags[nodeKey]).map((v) => v.toLowerCase());
    return values.some((v) => nodeVals.includes(v.toLowerCase()));
  });
}

/** True when a node satisfies every active filter category (text AND tags AND groups AND flagged). */
function nodeMatches(nodeId: string, graph: Graph, criteria: FilterCriteria): boolean {
  const node = graph.data_map[nodeId];
  if (!node) return false;
  if (criteria.text && !getNodeName(node, 1000).toLowerCase().includes(criteria.text.toLowerCase())) return false;
  if (Object.keys(criteria.tags).length > 0 && !matchesTags(getNodeTags(node), criteria.tags)) return false;
  if (criteria.groups.length > 0) {
    const groups = nodeGroups(node).map((g) => g.toLowerCase());
    if (!criteria.groups.some((g) => groups.includes(g.toLowerCase()))) return false;
  }
  if (criteria.flaggedOnly && !criteria.flaggedNodes.has(nodeId)) return false;
  return true;
}

/**
 * Compute the set of node ids to render under an active filter: a node is visible if it matches the criteria,
 * or any of its (policy/depth-resolved) descendants does — so ancestors of matches stay reachable. Traverses
 * only the currently-loaded graph.
 *
 * @returns The set of visible node ids. When no filter is active, callers should skip filtering entirely.
 */
export function filterTree(rootIds: string[], index: TreeIndex, graph: Graph, criteria: FilterCriteria, cfg: TraversalConfig): Set<string> {
  const visible = new Set<string>();
  // Memoize each node's "does it or a descendant match" answer, keyed by (id, arrival context), because
  // reverse edges make a node's children depend on how it was reached. `inProgress` guards cycles. This keeps
  // the pass O(V × 2 × REVERSE_MAX) instead of exploding across the many paths bidirectional edges create —
  // the depth bound (`maxDepth`) is null on the file tab and cannot be relied on to bound cost.
  const memo = new Map<string, boolean>();
  const inProgress = new Set<string>();

  const dfs = (nodeId: string, viaReversed: boolean, reverseDepth: number): boolean => {
    const memoKey = `${nodeId}:${viaReversed ? 1 : 0}:${reverseDepth}`;
    const cached = memo.get(memoKey);
    if (cached !== undefined) {
      if (cached) visible.add(nodeId);
      return cached;
    }
    if (inProgress.has(memoKey)) return false; // cycle: contribute nothing on this arc
    inProgress.add(memoKey);
    let subtreeMatch = false;
    // pass the node's own id as the cycle-guard path; the memo (not a full ancestor path) bounds the DFS,
    // matching the same reverse-context the renderer uses
    for (const child of effectiveChildren(nodeId, index, graph, cfg, new Set([nodeId]), reverseDepth, viaReversed)) {
      if (dfs(child.edge.id, child.viaReversed ?? false, child.reverseDepth ?? 0)) subtreeMatch = true;
    }
    inProgress.delete(memoKey);
    const result = nodeMatches(nodeId, graph, criteria) || subtreeMatch;
    memo.set(memoKey, result);
    if (result) visible.add(nodeId);
    return result;
  };

  for (const rootId of rootIds) {
    dfs(rootId, false, 0);
  }
  return visible;
}

/**
 * Read the `Show`/`Hide`/`Exclude`/`Include` omnibar clauses into a layer config. Clause values are raw
 * {@link NodeType} enum values, so they map straight to policy keys. When a type appears under multiple
 * verbs, Exclude wins over Hide over Show.
 */
export function getEntityLayerConfigFromClauses(clauses: Clause[]): EntityLayerConfig {
  const policies: EntityLayerConfig['policies'] = {};
  for (const k of getStringFieldListFromClauses(clauses, 'Show')) policies[k as NodeType] = LayerPolicy.Show;
  for (const k of getStringFieldListFromClauses(clauses, 'Hide')) policies[k as NodeType] = LayerPolicy.PassThrough;
  for (const k of getStringFieldListFromClauses(clauses, 'Exclude')) policies[k as NodeType] = LayerPolicy.Skip;
  const include = getStringFieldListFromClauses(clauses, 'Include');
  return { policies, includeSet: include.length ? new Set(include as NodeType[]) : null };
}

/** Read the traversal `depth` from the omnibar clauses (last valid positive integer), else the default. */
export function getDepthFromClauses(clauses: Clause[], dflt = 1): number {
  const values = getStringFieldListFromClauses(clauses, 'depth')
    .map(Number)
    .filter((n) => Number.isInteger(n) && n > 0);
  return values.length ? values[values.length - 1] : dflt;
}

/** Collect the tag key→values present anywhere in the pulled graph, for the omnibar tag options. */
export function collectTagOptions(graph: Graph): TagOptions {
  // accumulate into per-key Sets so each value is deduped in O(1); materializing the arrays once at the end
  // avoids the O(n·tags) rebuild that copying `out[key]` into a fresh Set per node would incur
  const accum = new Map<string, Set<string>>();
  for (const node of Object.values(graph.data_map ?? {})) {
    for (const [key, values] of Object.entries(getNodeTags(node))) {
      let set = accum.get(key);
      if (!set) {
        set = new Set();
        accum.set(key, set);
      }
      for (const value of Object.keys(values ?? {})) set.add(value);
    }
  }
  const out: TagOptions = {};
  for (const [key, set] of accum) out[key] = Array.from(set);
  return out;
}

/** Collect the groups present on any node in the pulled graph, for the omnibar group options. */
export function collectGroupOptions(graph: Graph): string[] {
  const set = new Set<string>();
  for (const node of Object.values(graph.data_map ?? {})) {
    for (const group of nodeGroups(node)) set.add(group);
  }
  return Array.from(set).sort();
}

/**
 * Compute the "flagged" node set: nodes with danger tags, every `Flag` entity node, and every ancestor of a
 * Flag node (reverse-BFS over `parentsOf`) — i.e. anything with a Flag reachable through its associations
 * within the pulled tree (bounded by traversed depth). Danger tags do not propagate to ancestors.
 *
 * @param graph - The shared graph.
 * @param index - The tree index (for `parentsOf`).
 * @returns The set of flagged node ids.
 */
export function computeFlaggedNodes(graph: Graph, index: TreeIndex): Set<string> {
  const flagged = new Set<string>();
  const flagSeeds: string[] = [];
  for (const [id, node] of Object.entries(graph.data_map ?? {})) {
    if (node[TreeNodeKey.Entity]?.kind === Entities.Flag) {
      flagSeeds.push(id);
      flagged.add(id);
    }
    if (hasDangerTags(getNodeTags(node))) flagged.add(id);
  }
  // walk up from every Flag node so each thing it's (transitively) associated to counts as flagged
  const queue = [...flagSeeds];
  let i = 0;
  while (i < queue.length) {
    const current = queue[i++];
    for (const parent of parentIdsOf(index, current)) {
      if (!flagged.has(parent)) {
        flagged.add(parent);
        queue.push(parent);
      }
    }
  }
  return flagged;
}
