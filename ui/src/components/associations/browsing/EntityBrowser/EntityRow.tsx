// spec: ./EntityBrowser.spec.md
import React, { useMemo, useState } from 'react';
import { FaBullseye, FaChevronRight, FaEyeSlash, FaSeedling } from 'react-icons/fa6';

// project imports
import { effectiveChildren, getDisplayTags, getNodeTags, hasDangerTags, nodeTypeOf } from './browserHelpers';
import EntityTreeLevel from './EntityTreeLevel';
import MetadataBox from './MetadataBox';
import { useEntityBrowser } from './EntityBrowserContext';
import {
  BadgeGroup,
  Chevron,
  DangerDot,
  DepthPill,
  DuplicateBadge,
  FocusButton,
  GrowBadge,
  HeaderIdentity,
  HeaderLead,
  HeaderTrail,
  HideButton,
  HideSlot,
  INDENT_CAP,
  IdentifierLink,
  IdentifierText,
  InfoBox,
  KindBadge,
  RelationshipBadge,
  RowContainer,
  RowHeader,
  RowSpinner,
  TagBadge,
  TagBadgeGroup,
  TagBadgeKey,
  TagBadgeValue,
  TagOverflowBadge,
  ViaBadge,
} from './EntityBrowser.styled';
import { hasContextualDisplayChildren, toDisplayCfg, TreeEdge } from '../treeHelpers';
import { FocusSource, useGraphData } from '../../data/GraphDataContext';
import { getNodeName } from '../../utilities';
import EntityTypeIcon from '@components/entities/shared/EntityTypeIcon';
import { OverlayTipTop } from '@components/shared/overlay/tips';
import { treeNodeToInfo } from '@components/shared/info/info';
import { Entities, entityLabel } from '@models/entities';

interface EntityRowProps {
  nodeId: string;
  /** Unique per occurrence (path-derived) so DAG duplicates expand independently. */
  rowKey: string;
  /** Ancestor node ids on the path to (and including) this row's parent — for the children's cycle guard. */
  path: Set<string>;
  depth: number;
  /** The incoming edge (for the relationship badge); omitted for roots. */
  edge?: TreeEdge;
  /** Names of pass-through layers elided above this row. */
  breadcrumb?: string[];
  /** Arrival context: how many reversed (against-direction) hops preceded this row. */
  reverseDepth?: number;
  /** Arrival context: whether this row was itself reached via a reversed edge. */
  viaReversed?: boolean;
}

/**
 * A single node as an "info box": a header (clicking it expands the node's associated CHILDREN — and lazily
 * grows the shared graph once if growable) plus, directly under it, a condensed "details" caret that reveals
 * the node's metadata on demand. Clicking the name navigates to that resource's details page. Nested
 * associations render below the box when expanded.
 */
const EntityRow: React.FC<EntityRowProps> = ({ nodeId, rowKey, path, depth, edge, breadcrumb, reverseDepth = 0, viaReversed = false }) => {
  const { graph, growable, grow, setFocusedNode } = useGraphData();
  const browser = useEntityBrowser();
  const [growing, setGrowing] = useState(false);

  const node = graph.data_map[nodeId];
  const info = useMemo(() => (node ? treeNodeToInfo(node) : null), [node]);
  // descriptive tag chips for the header (capped, noise keys dropped); recomputed only when the node changes
  const displayTags = useMemo(() => (node ? getDisplayTags(node) : { shown: [], overflow: 0, overflowLabels: [] }), [node]);
  const nodeType = nodeTypeOf(nodeId, graph);
  const childrenExpanded = browser.isChildrenExpanded(rowKey, nodeId, viaReversed, reverseDepth);
  const isDuplicate = browser.multiParent.has(nodeId);
  const isDanger = node ? hasDangerTags(getNodeTags(node)) : false;
  const isGrowable = growable.has(nodeId);
  // expandable when growable OR it has any display child in THIS arrival context (forward children, plus
  // reverse relationship edges bounded by the reverse-depth rules). When some children are hidden we fall back
  // to effectiveChildren (which drops hidden subtrees) so a parent whose only children are hidden shows no
  // chevron; the cheap contextual check suffices when nothing is hidden.
  const hasHidden = browser.hiddenNodes.size > 0;
  const canExpand = useMemo(() => {
    if (isGrowable) return true;
    if (!hasHidden) {
      return hasContextualDisplayChildren(browser.index, nodeId, toDisplayCfg(browser.traversalConfig), viaReversed, reverseDepth);
    }
    return (
      effectiveChildren(nodeId, browser.index, graph, browser.traversalConfig, new Set([nodeId]), reverseDepth, viaReversed).length > 0
    );
  }, [isGrowable, hasHidden, browser.index, browser.traversalConfig, nodeId, graph, viaReversed, reverseDepth]);

  const pathWithSelf = useMemo(() => {
    const next = new Set(path);
    next.add(nodeId);
    return next;
  }, [path, nodeId]);

  const onToggleChildren = () => {
    const willExpand = !childrenExpanded;
    browser.setChildrenExpanded(rowKey, willExpand);
    // selecting a row drives the side-by-side association graph to focus this node (see spec: row → graph focus)
    setFocusedNode(nodeId, FocusSource.Tree);
    // grow the shared graph once when first descending into a growable node
    if (willExpand && isGrowable && !browser.grownNodes.has(nodeId)) {
      browser.grownNodes.add(nodeId);
      setGrowing(true);
      void grow(nodeId).finally(() => setGrowing(false));
    }
  };
  // hide this node and its whole subtree from the tree (entities view only); stop propagation so the header's
  // toggle/focus doesn't also fire
  const onHide = (e: React.MouseEvent) => {
    e.stopPropagation();
    browser.hideNode(nodeId);
  };
  // re-root the tree at this node (focus its subtree); stop propagation so the header's toggle doesn't also fire
  const onFocus = (e: React.MouseEvent) => {
    e.stopPropagation();
    browser.setFocusRoot(nodeId);
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      onToggleChildren();
    }
  };

  // getNodeName returns '' for unnamed nodes, which `??` would keep; fall through with `||` to the node id
  const title = info?.title || (node ? getNodeName(node, 80) : '') || nodeId;

  return (
    <RowContainer>
      <InfoBox data-testid="entity-infobox">
        <RowHeader
          role="button"
          tabIndex={0}
          data-testid="entity-row"
          data-node-id={nodeId}
          aria-expanded={childrenExpanded}
          onClick={onToggleChildren}
          onKeyDown={onKeyDown}
        >
          <Chevron $expanded={childrenExpanded} $hidden={!canExpand}>
            <FaChevronRight size={10} />
          </Chevron>
          <HeaderLead>
            <HeaderIdentity>
              <EntityTypeIcon kind={nodeType as Entities} size={14} />
              {info?.titleHref ? (
                <IdentifierLink href={info.titleHref} target="_blank" rel="noreferrer" onClick={(e) => e.stopPropagation()}>
                  {title}
                </IdentifierLink>
              ) : (
                <IdentifierText>{title}</IdentifierText>
              )}
            </HeaderIdentity>
            <BadgeGroup>
              <KindBadge>{entityLabel(nodeType)}</KindBadge>
              {edge && (
                <RelationshipBadge title={edge.containerLabel ? `${edge.label} ${edge.containerLabel}` : edge.label}>
                  {edge.label}
                  {edge.containerLabel ? ` ${edge.containerLabel}` : ''}
                </RelationshipBadge>
              )}
              {breadcrumb && breadcrumb.length > 0 && (
                <ViaBadge title={`Reached through: ${breadcrumb.join(' › ')}`}>via {breadcrumb.join(' › ')}</ViaBadge>
              )}
              {isDanger && <DangerDot title="Carries danger-classified tags" />}
              {isDuplicate && <DuplicateBadge title="Appears under multiple parents in this tree">Duplicate</DuplicateBadge>}
              {isGrowable && (
                <GrowBadge title="More associations can be loaded — expand to grow">
                  <FaSeedling size={10} /> grow
                </GrowBadge>
              )}
            </BadgeGroup>
            {displayTags.shown.length > 0 && (
              <TagBadgeGroup>
                {displayTags.shown.map((tag) => (
                  <TagBadge key={`${tag.key}:${tag.value}`} title={`${tag.key}: ${tag.value}`}>
                    <TagBadgeKey>{tag.key}</TagBadgeKey>
                    <TagBadgeValue>{tag.value}</TagBadgeValue>
                  </TagBadge>
                ))}
                {displayTags.overflow > 0 && (
                  <TagOverflowBadge title={displayTags.overflowLabels.join('\n')}>+{displayTags.overflow}</TagOverflowBadge>
                )}
              </TagBadgeGroup>
            )}
          </HeaderLead>
          <HeaderTrail>
            {growing && <RowSpinner aria-label="loading" />}
            {/* focus (re-root) affordance: hover-revealed on shallow rows; once nesting passes the indent cap
                it promotes to an always-visible depth pill (the pill doubles as the focus trigger). Only shown
                when the row actually has a subtree to focus into. */}
            {canExpand &&
              (depth > INDENT_CAP ? (
                <OverlayTipTop tip="Focus on this subtree (reset the nesting here)">
                  <DepthPill type="button" aria-label={`Focus on ${title} — nested ${depth} levels deep`} onClick={onFocus}>
                    <FaBullseye size={10} aria-hidden /> {depth}
                  </DepthPill>
                </OverlayTipTop>
              ) : (
                <OverlayTipTop tip="Focus on this subtree">
                  <FocusButton type="button" aria-label={`Focus on ${title} — show only this subtree`} onClick={onFocus}>
                    <FaBullseye size={13} />
                  </FocusButton>
                </OverlayTipTop>
              ))}
            <HideSlot>
              <OverlayTipTop tip="Hide this item and everything under it">
                <HideButton type="button" aria-label={`Hide ${title} and everything under it`} onClick={onHide}>
                  <FaEyeSlash size={13} />
                </HideButton>
              </OverlayTipTop>
            </HideSlot>
          </HeaderTrail>
        </RowHeader>
        {info && <MetadataBox model={info} />}
      </InfoBox>
      {childrenExpanded && (
        <EntityTreeLevel
          parentId={nodeId}
          path={pathWithSelf}
          depth={depth + 1}
          rowKeyPrefix={rowKey}
          explicit={browser.isChildrenExplicit(rowKey)}
          reverseDepth={reverseDepth}
          viaReversed={viaReversed}
        />
      )}
    </RowContainer>
  );
};

// memoized so a parent re-render (e.g. an unrelated expand elsewhere in the tree) doesn't re-render every row;
// the row still re-renders when its own props change or the context values it reads update
export default React.memo(EntityRow);
