import { describe, expect, it } from 'vitest';

// project imports
import { decorationWidth, DECORATION_ICON_WIDTH } from './decorations';
import { estimateStageWidth } from './order';
import { orderToNodesAndEdges } from './PipelineOrderFlow';

describe('decorationWidth', () => {
  it('adds nothing to a plain diagram', () => {
    expect(decorationWidth()).toBe(0);
    expect(decorationWidth({ scan: { selected: true } })).toBe(0);
  });
  it('makes room for an icon', () => {
    expect(decorationWidth({ scan: { icon: 'x' } })).toBe(DECORATION_ICON_WIDTH);
  });
});

describe('orderToNodesAndEdges with decorations', () => {
  it('keeps the plain layout when no decorations are given', () => {
    const { nodes } = orderToNodesAndEdges([['scan']]);
    const step = nodes.find((node) => node.id === 'step-0-0');
    expect(step?.style?.width).toBe(estimateStageWidth(['scan']));
    expect(step?.data).not.toHaveProperty('decoration', expect.anything());
  });
  it('hands each stage its decoration and the select callback, and widens the nodes for the icon', () => {
    const onSelect = () => undefined;
    const { nodes } = orderToNodesAndEdges(
      [['scan'], ['a', 'b']],
      undefined,
      { scan: { icon: 'i', selected: true }, b: { active: true } },
      onSelect,
    );
    const byId = Object.fromEntries(nodes.map((node) => [node.id, node]));
    expect(byId['step-0-0'].style?.width).toBe(estimateStageWidth(['scan']) + DECORATION_ICON_WIDTH);
    expect((byId['step-0-0'].data as { decoration?: { selected?: boolean } }).decoration?.selected).toBe(true);
    expect((byId['step-1-1'].data as { decoration?: { active?: boolean } }).decoration?.active).toBe(true);
    expect((byId['step-1-0'].data as { onSelect?: unknown }).onSelect).toBe(onSelect);
  });
  it('keeps node ids stable when only the decorations change', () => {
    const order = [['scan'], ['a', 'b']];
    const first = orderToNodesAndEdges(order, undefined, { scan: { icon: 'i' } }).nodes.map((node) => [node.id, node.position]);
    const second = orderToNodesAndEdges(order, undefined, { scan: { icon: 'j', active: true }, a: { icon: 'k' } }).nodes.map((node) => [
      node.id,
      node.position,
    ]);
    expect(second).toEqual(first);
  });
});
