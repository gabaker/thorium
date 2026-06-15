// spec: ./EntityBrowser.spec.md
import React from 'react';

// project imports
import { getNodeTags, hasDangerTags } from './browserHelpers';
import { AggBadge, GroupCount, GroupHeaderRow } from './EntityBrowser.styled';
import { EffectiveChild } from './types';
import EntityTypeIcon from '@components/entities/shared/EntityTypeIcon';
import { useGraphData } from '../../data/GraphDataContext';
import { bucketTags, countTagValues } from '@components/tags/utilities';
import { Entities, entityLabel } from '@models/entities';
import { NodeType } from '@models/trees';

interface LayerHeaderProps {
  nodeType: NodeType;
  /** All children in this kind group (full group, even when the level paginates the rows below). */
  groupChildren: EffectiveChild[];
}

/**
 * Group header for a layer (node kind) at one tree level: the kind icon + label + member count, plus
 * aggregate significance badges (how many members carry danger tags, and total ATT&CK / MBC techniques).
 * A trailing `+` on the count flags that some members are growable-but-ungrown, so the count is a floor.
 */
const LayerHeader: React.FC<LayerHeaderProps> = ({ nodeType, groupChildren }) => {
  const { graph, growable } = useGraphData();

  let dangerNodes = 0;
  let attack = 0;
  let mbc = 0;
  let anyGrowable = false;
  for (const child of groupChildren) {
    if (growable.has(child.edge.id)) anyGrowable = true;
    const node = graph.data_map[child.edge.id];
    if (!node) continue;
    const buckets = bucketTags(getNodeTags(node));
    if (hasDangerTags(getNodeTags(node))) dangerNodes += 1;
    attack += countTagValues(buckets.attack);
    mbc += countTagValues(buckets.mbc);
  }

  return (
    <GroupHeaderRow>
      <EntityTypeIcon kind={nodeType as Entities} size={13} />
      <span>{entityLabel(nodeType)}</span>
      <GroupCount>
        {groupChildren.length}
        {anyGrowable ? '+' : ''}
      </GroupCount>
      {dangerNodes > 0 && (
        <AggBadge $danger title={`${dangerNodes} with danger-classified tags`}>
          ⚠ {dangerNodes}
        </AggBadge>
      )}
      {attack > 0 && <AggBadge title="ATT&CK techniques">ATT&CK {attack}</AggBadge>}
      {mbc > 0 && <AggBadge title="MBC behaviors">MBC {mbc}</AggBadge>}
    </GroupHeaderRow>
  );
};

export default LayerHeader;
