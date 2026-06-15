// spec: ./EntityBrowser.spec.md
import React, { Fragment, useMemo, useState } from 'react';

// project imports
import { effectiveChildren, groupByKind } from './browserHelpers';
import EntityRow from './EntityRow';
import LayerHeader from './LayerHeader';
import { useEntityBrowser } from './EntityBrowserContext';
import { EmptyNote, Level, ShowMoreButton, ShowMoreRow } from './EntityBrowser.styled';
import { useGraphData } from '../../data/GraphDataContext';

// how many rows to render per level before a "show more" control (keeps noisy graphs responsive)
export const PAGE_SIZE = 25;

interface EntityTreeLevelProps {
  parentId: string;
  /** Ancestor node ids on the path to (and including) `parentId` — for the cycle guard. */
  path: Set<string>;
  depth: number;
  /** The parent row's key; child row keys are derived from it to stay path-unique. */
  rowKeyPrefix: string;
  /**
   * Whether the parent was explicitly expanded by the user (vs depth-driven auto-expand). Only an explicit
   * expand surfaces the "No further associations" empty note — auto-expanded leaves stay quiet.
   */
  explicit?: boolean;
  /** Arrival context of `parentId`: how many reversed (against-direction) hops preceded it. */
  reverseDepth?: number;
  /** Arrival context of `parentId`: whether it was itself reached via a reversed edge. */
  viaReversed?: boolean;
}

/**
 * Renders one parent's effective children: policy-resolved (Skip/PassThrough/Show), optionally filtered,
 * grouped by kind under {@link LayerHeader}s (only when a header adds value), and paginated per level.
 */
const EntityTreeLevel: React.FC<EntityTreeLevelProps> = ({
  parentId,
  path,
  depth,
  rowKeyPrefix,
  explicit,
  reverseDepth = 0,
  viaReversed = false,
}) => {
  const { graph, growable } = useGraphData();
  const browser = useEntityBrowser();
  const [limit, setLimit] = useState(PAGE_SIZE);

  const children = useMemo(
    () => effectiveChildren(parentId, browser.index, graph, browser.traversalConfig, path, reverseDepth, viaReversed),
    [browser.index, browser.traversalConfig, parentId, path, graph, reverseDepth, viaReversed],
  );

  const filtered = browser.visibleSet ? children.filter((c) => browser.visibleSet!.has(c.edge.id)) : children;

  if (filtered.length === 0) {
    // a filter that hides everything always explains itself; otherwise only an explicit expand shows the
    // empty note (auto-expanded leaves render nothing so the tree stays quiet)
    if (browser.visibleSet) {
      return (
        <Level $depth={depth}>
          <EmptyNote>No matching items.</EmptyNote>
        </Level>
      );
    }
    if (!explicit) return null;
    // a still-growable parent isn't fully explored — don't claim "no further associations" (its grow is
    // pending or available); the empty note is only accurate once the node is fully grown.
    if (growable.has(parentId)) return null;
    return (
      <Level $depth={depth}>
        <EmptyNote>No further associations.</EmptyNote>
      </Level>
    );
  }

  const groups = groupByKind(filtered, graph);
  // a header earns its row only when it disambiguates (multiple kinds) or summarizes (multiple members)
  const showHeaders = groups.length > 1 || filtered.length > 1;

  // paginate across the flattened child list while keeping group boundaries
  let rendered = 0;
  const groupEls: React.ReactNode[] = [];
  for (const group of groups) {
    if (rendered >= limit) break;
    const slice = group.children.slice(0, limit - rendered);
    rendered += slice.length;
    groupEls.push(
      <Fragment key={group.nodeType}>
        {showHeaders && <LayerHeader nodeType={group.nodeType} groupChildren={group.children} />}
        {slice.map((child) => (
          <EntityRow
            key={`${rowKeyPrefix}/${child.edge.id}`}
            nodeId={child.edge.id}
            edge={child.edge}
            breadcrumb={child.breadcrumb}
            rowKey={`${rowKeyPrefix}/${child.edge.id}`}
            path={path}
            depth={depth}
            reverseDepth={child.reverseDepth ?? 0}
            viaReversed={child.viaReversed ?? false}
          />
        ))}
      </Fragment>,
    );
  }

  const remaining = filtered.length - rendered;

  return (
    <Level $depth={depth}>
      {groupEls}
      {remaining > 0 && (
        <ShowMoreRow>
          <ShowMoreButton onClick={() => setLimit((l) => l + PAGE_SIZE)}>Show more ({remaining} remaining)</ShowMoreButton>
        </ShowMoreRow>
      )}
    </Level>
  );
};

// memoized so an expand/collapse elsewhere in the tree doesn't re-render every level; the level re-renders on
// its own prop changes (parentId/path/depth/context) and its local "show more" state
export default React.memo(EntityTreeLevel);
