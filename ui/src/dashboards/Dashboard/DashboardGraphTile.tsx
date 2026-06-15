import React, { Suspense } from 'react';

// spec: ./SPEC.md

// project imports
import { GraphContentTile, GraphFallbackContainer, TileHeader } from './styles';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';

/**
 * The association graph, lazily loaded so its heavy 3D bundle is only fetched when a dashboard that
 * shows the graph mounts.
 */
const AssociationGraph = React.lazy(() => import('@components/associations/graph/AssociationGraph'));

/// Centered spinner shown while the lazy graph chunk loads.
const GraphFallback = (
  <GraphFallbackContainer>
    <LoadingSpinner loading />
  </GraphFallbackContainer>
);

/// Props for {@link DashboardGraphTile}.
export interface DashboardGraphTileProps {
  /**
   * Whether the tile is currently shown (its tab is active, or it is always visible on ultra-wide
   * layouts). Drives `AssociationGraph`'s `inView` so the 3D canvas never renders behind a hidden
   * tab — the plan prefers this explicit signal over `useInView({ triggerOnce: true })`.
   */
  active: boolean;
}

/**
 * The dashboard's graph tile: a lazily-loaded {@link AssociationGraph} whose rendering is gated on
 * the `active` prop.
 *
 * The tile itself stays mounted (its wrapper is toggled with `display: none` by the composing page so
 * layout/state is preserved), but `AssociationGraph` only mounts its 3D scene while `active` is true;
 * `AssociationGraph` internally renders nothing when `inView` is false, so an inactive tab pays no
 * WebGL cost.
 *
 * @param active - Whether the graph should render (visible tab / ultra-wide layout).
 * @returns The graph tile.
 */
const DashboardGraphTile: React.FC<DashboardGraphTileProps> = ({ active }) => (
  <GraphContentTile>
    <TileHeader>Association Graph</TileHeader>
    <Suspense fallback={GraphFallback}>
      <AssociationGraph inView={active} />
    </Suspense>
  </GraphContentTile>
);

export default DashboardGraphTile;
