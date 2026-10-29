import React from 'react';
import { describe, expect, it, vi } from 'vitest';

// project imports
import { readSweep, usesRefresh } from './RunBrowsing';
import { getReaction } from '@thorpi/reactions';
import { listRunsByStatus, listRunsByTag } from '@thorpi/runs';
import { Reaction, ReactionStatus } from '@models/reactions';

// the page shell needs the auth provider, which is not under test here
vi.mock('@components/pages/Page', () => ({
  default: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));
// the listings and the reads by id are answered by each test instead of the API
vi.mock('@thorpi/runs', () => ({ listRunsByStatus: vi.fn(), listRunsByTag: vi.fn() }));
vi.mock('@thorpi/reactions', () => ({ getReaction: vi.fn() }));

describe('RunBrowsing refresh sweep', () => {
  it('uses the refreshed pages when every listing answers, however many re-reads fail', async () => {
    // the first page of the Completed listing holds a run shown as Started; every other listing is empty
    const completed = { id: 'done', group: 'g', sla: '2026-01-01', status: ReactionStatus.Completed } as unknown as Reaction;
    vi.mocked(listRunsByTag).mockResolvedValue({ runs: [], cursor: null });
    vi.mocked(listRunsByStatus).mockImplementation((_group, status) =>
      Promise.resolve({ runs: status === ReactionStatus.Completed ? [completed] : [], cursor: null }),
    );
    // every stale row read again by id answers 500
    vi.mocked(getReaction).mockImplementation((_group, id, onError, onStatus) => {
      onError(`Get Reaction: ${id}`);
      onStatus?.(500);
      return Promise.resolve(null);
    });
    // more stale unfinished rows are shown than there are listings, so their failures outnumber the listings
    const stale = Array.from({ length: 20 }, (_, index) => ({
      id: `stale-${index}`,
      group: 'g',
      sla: '2026-01-01',
      status: ReactionStatus.Started,
    }));
    const shown = [{ ...completed, status: ReactionStatus.Started }, ...stale] as unknown as Reaction[];
    const errors: string[] = [];
    const sweep = await readSweep(['g'], shown, (message) => errors.push(message));
    expect(errors).toHaveLength(20);
    expect(usesRefresh(sweep)).toBe(true);
    expect(sweep.failures).toBe(0);
    expect(sweep.rereadFailures).toBe(20);
  });
});
