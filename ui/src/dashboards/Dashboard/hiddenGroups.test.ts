import { describe, expect, it } from 'vitest';

// project imports
import { groupHiddenByType } from './hiddenGroups';
import { NodeType } from '@models/trees';

// simple type/label stubs so the pure grouping logic can be tested without a graph
const TYPE_BY_ID: Record<string, NodeType> = {
  f1: NodeType.File,
  f2: NodeType.File,
  r1: NodeType.Repo,
  d1: NodeType.Device,
};
const typeOf = (id: string): NodeType => TYPE_BY_ID[id] ?? NodeType.Other;
const labelOf = (type: NodeType): string => String(type);

describe('groupHiddenByType', () => {
  it('returns an empty array for no ids', () => {
    expect(groupHiddenByType([], typeOf, labelOf)).toEqual([]);
  });

  it('buckets ids by type, preserving first-seen order within a group', () => {
    const groups = groupHiddenByType(['f1', 'r1', 'f2'], typeOf, labelOf);
    const file = groups.find((g) => g.type === NodeType.File);
    expect(file?.ids).toEqual(['f1', 'f2']);
    const repo = groups.find((g) => g.type === NodeType.Repo);
    expect(repo?.ids).toEqual(['r1']);
  });

  it('orders groups alphabetically by label', () => {
    const groups = groupHiddenByType(['r1', 'd1', 'f1'], typeOf, labelOf);
    expect(groups.map((g) => g.label)).toEqual([...groups.map((g) => g.label)].sort((a, b) => a.localeCompare(b)));
  });

  it('falls back to the resolved type for unknown ids', () => {
    const groups = groupHiddenByType(['unknown'], typeOf, labelOf);
    expect(groups).toHaveLength(1);
    expect(groups[0].type).toBe(NodeType.Other);
    expect(groups[0].ids).toEqual(['unknown']);
  });
});
