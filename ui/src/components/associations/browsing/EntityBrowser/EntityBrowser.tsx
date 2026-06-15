// spec: ./EntityBrowser.spec.md
import React, { useMemo } from 'react';

// project imports
import BrowserToolbar from './BrowserToolbar';
import EntityRow from './EntityRow';
import EntityTreeLevel from './EntityTreeLevel';
import { EntityBrowserProvider, useEntityBrowser } from './EntityBrowserContext';
import { BrowserRoot } from './EntityBrowser.styled';
import { EntityBrowserProps } from './types';
import { useGraphData } from '../../data/GraphDataContext';
import AlertBanner, { Severity } from '@components/shared/alerts/AlertBanner';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';

/**
 * The rendered body of the browser (inside the provider): handles loading/empty/error states, then renders
 * the toolbar plus either the root rows or — when `showRootNodes` is false — the roots' children directly
 * (used by the file-details tab, where the file itself is implicit).
 */
const EntityBrowserBody: React.FC<{ showRootNodes: boolean }> = ({ showRootNodes }) => {
  const { loading, error, graphVersion } = useGraphData();
  const { roots, visibleSet } = useEntityBrowser();

  // stable path set per root so nested levels don't recompute their children every render
  const rootPaths = useMemo(() => {
    const map = new Map<string, Set<string>>();
    for (const root of roots) map.set(root.id, new Set([root.id]));
    return map;
  }, [roots]);

  if (error) {
    return <AlertBanner severity={Severity.Error}>{error}</AlertBanner>;
  }
  // graphVersion is 0 until the shared provider's initial fetch resolves
  if (loading || graphVersion === 0) {
    return <LoadingSpinner loading={true} />;
  }
  if (roots.length === 0) {
    return <AlertBanner severity={Severity.Info}>No Associated Entities</AlertBanner>;
  }

  const shownRoots = visibleSet ? roots.filter((r) => visibleSet.has(r.id)) : roots;

  return (
    <BrowserRoot data-testid="entity-browser">
      <BrowserToolbar />
      {shownRoots.length === 0 ? (
        <AlertBanner severity={Severity.Info}>No matching items.</AlertBanner>
      ) : showRootNodes ? (
        shownRoots.map((root) => <EntityRow key={root.id} nodeId={root.id} rowKey={`root:${root.id}`} depth={0} path={new Set()} />)
      ) : (
        // file-tab mode: the file is implicit — render its associations directly as the top level
        shownRoots.map((root) => (
          <EntityTreeLevel
            key={root.id}
            parentId={root.id}
            path={rootPaths.get(root.id) ?? new Set([root.id])}
            depth={0}
            rowKeyPrefix={`root:${root.id}`}
            explicit
          />
        ))
      )}
    </BrowserRoot>
  );
};

/**
 * A generic, graph-driven browser of associated entities/files/repos/tags for a starting context. Reads the
 * shared {@link useGraphData} graph (so no extra fetch — growth converges with the association graph), and
 * lets the user filter, control per-layer visibility (show / pass-through / skip), expand into associations,
 * and inspect full metadata + tags inline.
 */
const EntityBrowser: React.FC<EntityBrowserProps> = ({
  roots,
  inView = true,
  defaultPolicies,
  fallbackPolicy,
  showRootNodes = true,
  defaultDepth,
}) => {
  if (inView === false) {
    return null;
  }
  return (
    <EntityBrowserProvider roots={roots} defaultPolicies={defaultPolicies} fallbackPolicy={fallbackPolicy} defaultDepth={defaultDepth}>
      <EntityBrowserBody showRootNodes={showRootNodes} />
    </EntityBrowserProvider>
  );
};

export default EntityBrowser;
