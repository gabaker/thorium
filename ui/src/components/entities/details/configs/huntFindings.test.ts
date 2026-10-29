import { describe, it, expect } from 'vitest';

// project imports
import { huntFindingsTabs, huntHitKind, huntHitsFilters, INLINE_HIT_HIDDEN_TAG_KEYS } from './huntFindings';
import { Entities } from '@models/entities';
import { BlankHunt, Hunt, HuntEngine, HUNT_ID_TAG } from '@models/entities/hunts';
import { SUBMITTER_TAG } from '@models/tags';

describe('huntFindingsTabs', () => {
  it('pins the tab keys, labels, and tips with hits leading as the default view', () => {
    expect(huntFindingsTabs(30)).toEqual([
      { key: 'hits', label: 'Hits', tip: 'The findings of this hunt' },
      { key: 'rules', label: 'Frozen Rules', count: 30, tip: 'The rules frozen into this hunt at creation' },
    ]);
  });

  it('badges the Frozen Rules tab with the exact snapshot size and never the Hits tab', () => {
    const tabs = huntFindingsTabs(2);
    // counters are telemetry and can drift under replay, so hits never shows a count
    expect(tabs.find((tab) => tab.key === 'hits')?.count).toBeUndefined();
    // the frozen snapshot is resolved at creation, so its size is exact
    expect(tabs.find((tab) => tab.key === 'rules')?.count).toBe(2);
  });
});

describe('huntHitKind', () => {
  it('resolves a yara hunt to the yara hit kind', () => {
    const hunt: Hunt = structuredClone(BlankHunt);
    hunt.id = '0a658ba7-78a9-4a2e-8df3-7fbf18960286';
    expect(huntHitKind(hunt)).toBe(Entities.YaraRuleHit);
  });

  it('returns undefined for the blank placeholder hunt', () => {
    // the details factory leaves BlankHunt in place until the hunt lands; an empty id
    // must never pin an empty HuntId filter
    expect(huntHitKind(structuredClone(BlankHunt))).toBeUndefined();
  });

  it('returns undefined for an engine this UI build does not know', () => {
    const hunt: Hunt = structuredClone(BlankHunt);
    hunt.id = '0a658ba7-78a9-4a2e-8df3-7fbf18960286';
    // a newer server can serve engines added after this UI shipped (rolling upgrades)
    hunt.metadata.Hunt.engine = 'sigma' as HuntEngine;
    expect(huntHitKind(hunt)).toBeUndefined();
  });
});

describe('huntHitsFilters', () => {
  it('pins the HuntId tag and the page size', () => {
    expect(huntHitsFilters('0a658ba7-78a9-4a2e-8df3-7fbf18960286', 25)).toEqual({
      tags: { HuntId: ['0a658ba7-78a9-4a2e-8df3-7fbf18960286'] },
      limit: 25,
    });
  });
});

describe('INLINE_HIT_HIDDEN_TAG_KEYS', () => {
  it('hides exactly the chips the panel already shows elsewhere', () => {
    // HuntId: every row in the panel carries the same hunt; submitter: a row column
    expect(INLINE_HIT_HIDDEN_TAG_KEYS).toEqual([HUNT_ID_TAG, SUBMITTER_TAG]);
  });
});
