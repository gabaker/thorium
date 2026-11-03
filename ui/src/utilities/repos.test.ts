import { describe, expect, it } from 'vitest';

// project imports
import { shortHash, splitCommitishes } from './repos';
import type { CommitishDetails } from '@models/repos';

describe('splitCommitishes', () => {
  it('splits by kind and sorts each kind newest first', () => {
    const list: CommitishDetails[] = [
      { Commit: { hash: 'a1', groups: [], timestamp: '2026-01-01T00:00:00Z', author: 'x', truncated: false } },
      { Branch: { name: 'dev', groups: [], commit: 'a1', timestamp: '2026-01-02T00:00:00Z' } },
      { Commit: { hash: 'b2', groups: [], timestamp: '2026-01-03T00:00:00Z', author: 'x', truncated: false } },
      { Tag: { name: 'v1', groups: [], commit: 'a1', author: 'x', timestamp: '2026-01-01T00:00:00Z' } },
      { Branch: { name: 'main', groups: [], commit: 'b2', timestamp: '2026-01-04T00:00:00Z' } },
    ];
    const split = splitCommitishes(list);
    expect(split.commits.map((c) => c.hash)).toEqual(['b2', 'a1']);
    expect(split.branches.map((b) => b.name)).toEqual(['main', 'dev']);
    expect(split.tags.map((t) => t.name)).toEqual(['v1']);
  });

  it('returns empty lists for no commitishes', () => {
    expect(splitCommitishes([])).toEqual({ branches: [], tags: [], commits: [] });
  });
});

describe('shortHash', () => {
  it('keeps the first 10 characters', () => {
    expect(shortHash('0123456789abcdef')).toBe('0123456789');
    expect(shortHash('abc')).toBe('abc');
  });
});
