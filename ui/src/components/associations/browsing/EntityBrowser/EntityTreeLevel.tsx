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
const PAGE_SIZE = 25;

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
}

/**
 * Renders one parent's effective children: policy-resolved (Skip/PassThrough/Show), optionally filtered,
 * grouped by kind under {@link LayerHeader}s (only when a header adds value), and paginated per level.
 */
const EntityTreeLevel: React.FC<EntityTreeLevelProps> = ({ parentId, path, depth, rowKeyPrefix, explicit }) => {
  const { graph } = useGraphData();
  const browser = useEntityBrowser();
  const [limit, setLimit] = useState(PAGE_SIZE);

  const children = useMemo(
    () => effectiveChildren(parentId, browser.index, graph, browser.traversalConfig, path),
    [browser.index, browser.traversalConfig, parentId, path, graph],
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

export default EntityTreeLevel;
