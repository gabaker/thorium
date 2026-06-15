import { describe, it, expect } from 'vitest';

// project imports
import { normalizeGraphIds } from './trees';
import { BlankGraph, Direction, Graph } from '@models/trees';

/**
 * Build a graph fixture from a partial override without repeating the blank base.
 *
 * @param patch - Fields to override on top of {@link BlankGraph}.
 * @returns A graph object safe to mutate in a single test.
 */
function graphWith(patch: Partial<Graph>): Graph {
  return { ...structuredClone(BlankGraph), ...structuredClone(patch) };
}

describe('normalizeGraphIds', () => {
  it('coerces numeric ids in initial/growable/sent to strings', () => {
    const graph = graphWith({
      initial: [123 as unknown as string],
      growable: [456 as unknown as string],
      sent: [789 as unknown as string],
    });
    const result = normalizeGraphIds(graph);
    expect(result.initial).toEqual(['123']);
    expect(result.growable).toEqual(['456']);
    expect(result.sent).toEqual(['789']);
  });

  it('coerces numeric branch and hint_branch node fields to strings', () => {
    const graph = graphWith({
      branches: {
        '1': [{ relationship: {}, node: 42 as unknown as string, direction: Direction.To, relationship_hash: 'h' }],
      },
      hint_branches: {
        '1': [{ relationship: {}, node: 99 as unknown as string, direction: Direction.From, relationship_hash: 'k' }],
      },
    });
    const result = normalizeGraphIds(graph);
    expect(result.branches['1'][0].node).toBe('42');
    expect(result.hint_branches?.['1'][0].node).toBe('99');
  });

  it('leaves string ids unchanged and tolerates absent optional fields', () => {
    const graph = graphWith({ initial: ['abc'], growable: ['def'] });
    delete graph.sent;
    delete graph.hint_branches;
    const result = normalizeGraphIds(graph);
    expect(result.initial).toEqual(['abc']);
    expect(result.growable).toEqual(['def']);
    expect(result.sent).toBeUndefined();
    expect(result.hint_branches).toBeUndefined();
  });
});
