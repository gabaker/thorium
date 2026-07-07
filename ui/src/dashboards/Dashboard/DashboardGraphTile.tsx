import React, { Suspense } from 'react';
import { FaCompress, FaExpand } from 'react-icons/fa';

// spec: ./SPEC.md

// project imports
import { GraphContentTile, GraphFallbackContainer, TileHeader, TileHeaderRow } from './styles';
import { IconButton } from '@components/shared/buttons';
import LoadingSpinner from '@components/shared/fallback/LoadingSpinner';
import { OverlayTipBottom } from '@components/shared/overlay/tips';

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
  /** Whether the focus (expand-to-fill) toggle is offered — only meaningful in the two-column ultra-wide layout. */
  canExpand?: boolean;
  /** Whether this pane is currently the focused (expanded) one, so the toggle shows the collapse affordance. */
  expanded?: boolean;
  /** Called when the focus toggle is clicked (focus this pane / restore the split view). */
  onToggleExpand?: () => void;
}

/**
 * The dashboard's graph tile: a lazily-loaded {@link AssociationGraph} whose rendering is gated on
 * the `active` prop.
 *
 * The tile itself stays mounted (its wrapper is toggled with `display: none` by the composing page so
 * layout/state is preserved), but `AssociationGraph` only mounts its 3D scene while `active` is true;
 * `AssociationGraph` internally renders nothing when `inView` is false, so an inactive tab pays no
 * WebGL cost. When `canExpand` is set the header carries a focus toggle that expands this pane to fill the
 * content region (and collapses back to the split view).
 *
 * @param active - Whether the graph should render (visible tab / ultra-wide layout).
 * @param canExpand - Whether to show the focus (expand-to-fill) toggle.
 * @param expanded - Whether this pane is currently focused (toggle shows the collapse icon).
 * @param onToggleExpand - Called when the focus toggle is clicked.
 * @returns The graph tile.
 */
const DashboardGraphTile: React.FC<DashboardGraphTileProps> = ({ active, canExpand = false, expanded = false, onToggleExpand }) => (
  <GraphContentTile>
    <TileHeader>
      <TileHeaderRow>
        <span>Association Graph</span>
        {canExpand && (
          <OverlayTipBottom tip={expanded ? 'Restore the split view' : 'Expand the graph to fill the dashboard'}>
            <IconButton onClick={onToggleExpand} aria-label={expanded ? 'Restore the split view' : 'Expand the graph to fill the dashboard'}>
              {expanded ? <FaCompress size={15} /> : <FaExpand size={15} />}
            </IconButton>
          </OverlayTipBottom>
        )}
      </TileHeaderRow>
    </TileHeader>
    <Suspense fallback={GraphFallback}>
      <AssociationGraph inView={active} />
    </Suspense>
  </GraphContentTile>
);

export default DashboardGraphTile;
