import { describe, it, expect } from 'vitest';

// project imports
import { canCancelHunt } from './CancelHuntButton';
import { BlankHunt, Hunt, HuntStatus } from '@models/entities/hunts';

/** Build a hunt with the given id and status on top of the blank defaults. */
function huntWith(id: string, status: HuntStatus): Hunt {
  const hunt = structuredClone(BlankHunt);
  hunt.id = id;
  hunt.metadata.Hunt.status = status;
  return hunt;
}

describe('canCancelHunt', () => {
  it('allows cancelling a loaded hunt that is still active', () => {
    expect(canCancelHunt(huntWith('0a658ba7-78a9-4a2e-8df3-7fbf18960286', HuntStatus.Created))).toBe(true);
    expect(canCancelHunt(huntWith('0a658ba7-78a9-4a2e-8df3-7fbf18960286', HuntStatus.Running))).toBe(true);
  });

  it('denies the blank placeholder even though its Created status counts as active', () => {
    // a missing or mismatched route id leaves BlankHunt in place; cancelling it would
    // POST /entities/hunt//cancel with an empty id
    expect(canCancelHunt(structuredClone(BlankHunt))).toBe(false);
  });

  it('denies a terminal hunt', () => {
    expect(canCancelHunt(huntWith('0a658ba7-78a9-4a2e-8df3-7fbf18960286', HuntStatus.Completed))).toBe(false);
    expect(canCancelHunt(huntWith('0a658ba7-78a9-4a2e-8df3-7fbf18960286', HuntStatus.Cancelled))).toBe(false);
    expect(canCancelHunt(huntWith('0a658ba7-78a9-4a2e-8df3-7fbf18960286', HuntStatus.Failed))).toBe(false);
  });
});
