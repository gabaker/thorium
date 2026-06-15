import React from 'react';

// spec: ./SPEC.md

// project imports
import { EntityBrowserBody } from '@components/associations/browsing/EntityBrowser/EntityBrowser';

/**
 * The dashboard's entity-browser tile.
 *
 * Renders {@link EntityBrowserBody} directly (no nested {@link EntityBrowserProvider}) so it relies on
 * the provider composed at the dashboard level — the Phase 1 provider seam — sharing the URL-backed
 * clause/hidden/flagged state with the dashboard's own omnibar strip. The dashboard supplies that strip
 * separately, so the body renders no toolbar (`toolbar={null}`).
 *
 * `roots={{ kind: 'initial' }}` is configured on the surrounding provider, not here; the body reads the
 * resolved roots from context. Root nodes are shown as their own expandable rows (`showRootNodes`) because
 * a dashboard has heterogeneous seeds with no single implicit artifact (unlike the file-details tab).
 *
 * @returns The entity-browser tile body.
 */
const BrowserTile: React.FC = () => <EntityBrowserBody showRootNodes toolbar={null} />;

export default React.memo(BrowserTile);
