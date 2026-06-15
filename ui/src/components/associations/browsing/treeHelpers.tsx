// project imports
import { classifyNode } from '../graph/data';
import { getEdgeLabel, getNodeName } from '../utilities';
import { CONTAINER_ASSOCIATION_KINDS } from '@models/associations';
import { entityLabel } from '@models/entities';
import { BranchNode, Direction, Graph, TreeRelationships } from '@models/trees';

// spec: ./AssociationTree.spec.md

/**
 * A single parent→child edge in a {@link TreeIndex}, carrying the graph's relationship metadata so
 * consumers (the entity browser's relationship badges) can describe *how* two nodes relate, not just that
 * they do. `direction` is the raw branch direction; `relationship` is the tagged relationship union
 * (Initial / Origin / Tags / Association); `label` is the pre-formatted display string.
 */
export interface TreeEdge {
  /** The child node id this edge points to. */
  id: string;
  /** The raw branch direction (To / From / Bidirectional). */
  direction: Direction;
  /** The relationship union for this edge (Association kind / Origin / Tags / Initial). */
  relationship: TreeRelationships;
  /** Pre-formatted, human-readable edge label (via {@link getEdgeLabel}). */
  label: string;
  /**
   * For containment ("… In …") associations, the container's `name kind` (e.g. "somefolder Folder"),
   * appended after {@link label} in the relationship badge. Undefined for non-containment edges.
   */
  containerLabel?: string;
}

export interface TreeIndex {
  /** Ordered, edge-carrying children per node. Use {@link childIdsOf} for a bare unique-id view. */
  childrenOf: Map<string, TreeEdge[]>;
  parentsOf: Map<string, string[]>;
}

/**
 * Build a direction-aware parent/child index from a graph's branches.
 *
 * Each branch is resolved to a parent→child edge by direction (To/Bidirectional ⇒ owner→node;
 * From ⇒ node→owner) and stored with its relationship metadata. Edges are deduped per parent by
 * (childId, relationship_hash): this collapses the reverse-pair a directed association produces (stored as
 * `To` on one endpoint and `From` on the other with the same hash) into a single edge, while still allowing
 * genuinely distinct relationships between the same two nodes to coexist. Bidirectional edges intentionally
 * yield *mutual* parent/child entries (A is a child of B and vice-versa); the browser's per-path cycle guard
 * keeps that from rendering forever.
 *
 * @param graph - The graph to index.
 * @returns An index of edge-carrying `childrenOf` and bare `parentsOf`.
 */
export function buildTreeIndex(graph: Graph): TreeIndex {
  const childrenOf = new Map<string, TreeEdge[]>();
  const parentsOf = new Map<string, string[]>();
  // dedupe keys per parent so reverse-pair branches don't produce duplicate edges
  const edgeKeys = new Map<string, Set<string>>();
  // built once and reused so per-edge classifyNode() doesn't reallocate the growable/initial sets
  const precomputed = {
    growableSet: new Set(graph.growable.map((n) => n.toString())),
    initialSet: new Set(graph.initial.map((n) => n.toString())),
  };

  const addEdge = (parent: string, child: string, branch: BranchNode) => {
    const key = `${child}-${branch.relationship_hash ?? ''}`;
    let keys = edgeKeys.get(parent);
    if (!keys) {
      keys = new Set();
      edgeKeys.set(parent, keys);
    }
    if (!keys.has(key)) {
      keys.add(key);
      let edges = childrenOf.get(parent);
      if (!edges) {
        edges = [];
        childrenOf.set(parent, edges);
      }
      // For containment ("… In …") associations the `parent` here is the association source = the container
      // (folder→file, filesystem→folder, file→filesystem — true for both To and From resolutions), so name it.
      const assocKind = branch.relationship.Association?.kind;
      let containerLabel: string | undefined;
      if (assocKind && CONTAINER_ASSOCIATION_KINDS.has(assocKind) && graph.data_map[parent]) {
        const name = getNodeName(graph.data_map[parent], 40);
        if (name) containerLabel = `${name} ${entityLabel(classifyNode(parent, graph, precomputed).nodeType)}`;
      }
      edges.push({
        id: child,
        direction: branch.direction,
        relationship: branch.relationship,
        // getEdgeLabel formats the label from the branch's relationship; the branch's target node is `child`
        label: getEdgeLabel(child, parent, branch, graph),
        containerLabel,
      });
    }

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
        // DEBUG (remove after diagnosing duplicate-node children bug): `branch.node` should be a
        // string, but json-bigint leaves u64 hashes <= 2^53 as JS numbers. A numeric `branch.node`
        // becomes a numeric key/edge-id here, which string-keyed lookups (growable Set, childrenOf.get)
        // later miss — so the node renders but its children never show.
        if (typeof branch.node !== 'string') {
          console.warn('[tree-debug] non-string branch.node in buildTreeIndex', {
            parent: nodeId,
            node: branch.node,
            nodeType: typeof branch.node,
            direction: branch.direction,
          });
        }
        // DEBUG (remove after diagnosing self-loop crash): a branch that points back to its own parent
        // creates a source===target link that can crash 3d-force-graph's OrbitControls.
        if (String(branch.node) === nodeId) {
          console.warn('[tree-debug] self-loop branch in buildTreeIndex', { node: nodeId, direction: branch.direction });
        }
        if (branch.direction === Direction.To || branch.direction === Direction.Bidirectional) {
          addEdge(nodeId, branch.node, branch);
        } else if (branch.direction === Direction.From) {
          addEdge(branch.node, nodeId, branch);
        }
      }
    }
  }
  // DEBUG (remove after diagnosing duplicate-node children bug): surface any numeric keys / edge ids
  // that ended up in the index. Any output here is the smoking gun — string-keyed consumers can't find these.
  const numericChildKeys = Array.from(childrenOf.keys()).filter((k) => typeof k !== 'string');
  const numericParentKeys = Array.from(parentsOf.keys()).filter((k) => typeof k !== 'string');
  const numericEdgeIds = Array.from(childrenOf.values())
    .flat()
    .map((e) => e.id)
    .filter((id) => typeof id !== 'string');
  if (numericChildKeys.length || numericParentKeys.length || numericEdgeIds.length) {
    console.warn('[tree-debug] numeric ids in tree index (root cause of missing children)', {
      numericChildKeys,
      numericParentKeys,
      numericEdgeIds,
    });
  }
  return { childrenOf, parentsOf };
}

/**
 * Bare, unique child-id view of a node's edges (preserves first-seen order).
 *
 * @param index - The tree index to read.
 * @param nodeId - The parent node id.
 * @returns The parent's child ids, each once, in edge order.
 */
export function childIdsOf(index: TreeIndex, nodeId: string): string[] {
  const edges = index.childrenOf.get(nodeId);
  if (!edges) return [];
  const seen = new Set<string>();
  const ids: string[] = [];
  for (const edge of edges) {
    if (!seen.has(edge.id)) {
      seen.add(edge.id);
      ids.push(edge.id);
    }
  }
  return ids;
}

export function findMultiParentNodeIds(graph: Graph, index?: TreeIndex): Set<string> {
  const idx = index ?? buildTreeIndex(graph);
  const multiParent = new Set<string>();
  for (const [nodeId, parents] of idx.parentsOf) {
    if (parents.length > 1) multiParent.add(nodeId);
  }
  return multiParent;
}
