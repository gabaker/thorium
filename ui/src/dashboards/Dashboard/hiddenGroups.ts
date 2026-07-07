// spec: ./SPEC.md

// project imports
import { NodeType } from '@models/trees';

/// A set of hidden node ids that share one resource type, for grouped display in the omnibar.
export interface HiddenGroup {
  /// The resource type shared by every id in this group.
  type: NodeType;
  /// The human-readable label for {@link type} (the group header).
  label: string;
  /// The hidden node ids of this type, in first-seen order.
  ids: string[];
}

/**
 * Group a flat list of hidden node ids by their resource type for display as labelled chip clusters.
 *
 * Kept pure (the type and label lookups are injected) so it is unit-testable without a graph: the caller
 * passes `nodeTypeOf(id, graph)` and `entityLabel`. Ids keep their first-seen order within a group, and
 * groups are ordered alphabetically by label so the layout is stable across renders.
 *
 * @param ids - The hidden node ids to group.
 * @param typeOf - Resolves a node id to its {@link NodeType}.
 * @param labelOf - Resolves a {@link NodeType} to its display label.
 * @returns The hidden ids bucketed by type, groups sorted by label.
 */
export function groupHiddenByType(
  ids: string[],
  typeOf: (id: string) => NodeType,
  labelOf: (type: NodeType) => string,
): HiddenGroup[] {
  // bucket ids by resolved type, preserving first-seen order within each bucket
  const byType = new Map<NodeType, string[]>();
  for (const id of ids) {
    const type = typeOf(id);
    const bucket = byType.get(type);
    if (bucket) {
      bucket.push(id);
    } else {
      byType.set(type, [id]);
    }
  }
  // materialize groups with their labels and order them by label for a stable layout
  return Array.from(byType, ([type, groupIds]) => ({ type, label: labelOf(type), ids: groupIds })).sort((a, b) =>
    a.label.localeCompare(b.label),
  );
}
