// spec: ./EntityBrowser.spec.md
import React, { useMemo, useState } from 'react';
import { FaChevronRight, FaSeedling } from 'react-icons/fa6';

// project imports
import { getNodeTags, hasDangerTags, nodeTypeOf } from './browserHelpers';
import EntityTreeLevel from './EntityTreeLevel';
import MetadataBox from './MetadataBox';
import { useEntityBrowser } from './EntityBrowserContext';
import {
  BadgeGroup,
  Chevron,
  DangerDot,
  DuplicateBadge,
  GrowBadge,
  IdentifierLink,
  IdentifierText,
  InfoBox,
  KindBadge,
  RelationshipBadge,
  RowContainer,
  RowHeader,
  RowSpinner,
  ViaBadge,
} from './EntityBrowser.styled';
import { TreeEdge } from '../treeHelpers';
import { useGraphData } from '../../data/GraphDataContext';
import { getNodeName } from '../../utilities';
import EntityTypeIcon from '@components/entities/shared/EntityTypeIcon';
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
}

/**
 * A single node as an "info box": a header (clicking it expands the node's associated CHILDREN — and lazily
 * grows the shared graph once if growable) plus, directly under it, a condensed "details" caret that reveals
 * the node's metadata on demand. Clicking the name navigates to that resource's details page. Nested
 * associations render below the box when expanded.
 */
const EntityRow: React.FC<EntityRowProps> = ({ nodeId, rowKey, path, depth, edge, breadcrumb }) => {
  const { graph, growable, grow } = useGraphData();
  const browser = useEntityBrowser();
  const [growing, setGrowing] = useState(false);

  const node = graph.data_map[nodeId];
  const info = node ? treeNodeToInfo(node) : null;
  const nodeType = nodeTypeOf(nodeId, graph);
  const childrenExpanded = browser.isChildrenExpanded(rowKey, nodeId);
  const isDuplicate = browser.multiParent.has(nodeId);
  const isDanger = node ? hasDangerTags(getNodeTags(node)) : false;
  const isGrowable = growable.has(nodeId);
  const hasLoadedChildren = (browser.index.childrenOf.get(nodeId)?.length ?? 0) > 0;
  const canExpand = isGrowable || hasLoadedChildren;
  // DEBUG (remove after diagnosing duplicate-node children bug): if `nodeId` arrives as a JS number,
  // the node still renders (data_map property access coerces number->string) but growable.has() and
  // childrenOf.get() (both string-keyed) miss, so canExpand is wrongly false. `viaStringKey` is the
  // child count for the SAME node looked up with a string key — non-zero here while canExpand is false
  // confirms the number/string mismatch is what hides the children.
  if (typeof (nodeId as unknown) !== 'string') {
    console.warn('[tree-debug] EntityRow nodeId is not a string', {
      nodeId,
      nodeType: typeof (nodeId as unknown),
      isGrowable,
      hasLoadedChildren,
      canExpand,
      hasData: !!node,
      viaStringKey: browser.index.childrenOf.get(String(nodeId))?.length ?? 0,
    });
  }

  const pathWithSelf = useMemo(() => {
    const next = new Set(path);
    next.add(nodeId);
    return next;
  }, [path, nodeId]);

  const onToggleChildren = () => {
    const willExpand = !childrenExpanded;
    browser.setChildrenExpanded(rowKey, willExpand);
    // grow the shared graph once when first descending into a growable node
    if (willExpand && isGrowable && !browser.grownNodes.has(nodeId)) {
      browser.grownNodes.add(nodeId);
      setGrowing(true);
      void grow(nodeId).finally(() => setGrowing(false));
    }
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      onToggleChildren();
    }
  };

  const title = info?.title ?? (node ? getNodeName(node, 80) : nodeId) ?? nodeId;

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
          <EntityTypeIcon kind={nodeType as Entities} size={14} />
          {info?.titleHref ? (
            <IdentifierLink href={info.titleHref} target="_blank" rel="noreferrer" onClick={(e) => e.stopPropagation()}>
              {title}
            </IdentifierLink>
          ) : (
            <IdentifierText>{title}</IdentifierText>
          )}
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
          {growing && <RowSpinner aria-label="loading" />}
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
        />
      )}
    </RowContainer>
  );
};

export default EntityRow;
