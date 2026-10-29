import { describe, it, expect } from 'vitest';

// project imports
import { buildHuntHitsHref } from './ListHuntHitsButton';
import { BlankHunt, Hunt, HuntEngine } from '@models/entities/hunts';

describe('buildHuntHitsHref', () => {
  it('links to the engine hit browse pre-filtered by the HuntId tag', () => {
    const hunt: Hunt = structuredClone(BlankHunt);
    hunt.id = '0a658ba7-78a9-4a2e-8df3-7fbf18960286';
    // the tags[HuntId] param is what the omnibar browse filters decode back into a clause
    expect(buildHuntHitsHref(hunt)).toBe('/rules/yara/hits?tags%5BHuntId%5D=0a658ba7-78a9-4a2e-8df3-7fbf18960286');
  });

  it('returns undefined for an engine this UI build does not know', () => {
    const hunt: Hunt = structuredClone(BlankHunt);
    hunt.id = '0a658ba7-78a9-4a2e-8df3-7fbf18960286';
    // a newer server can serve engines added after this UI shipped (rolling upgrades)
    hunt.metadata.Hunt.engine = 'sigma' as HuntEngine;
    expect(buildHuntHitsHref(hunt)).toBeUndefined();
  });

  it('returns undefined for the blank placeholder hunt', () => {
    // a missing or mismatched route id leaves BlankHunt in place; never browse an
    // empty HuntId filter
    expect(buildHuntHitsHref(structuredClone(BlankHunt))).toBeUndefined();
  });
});
