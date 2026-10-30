import { beforeEach, describe, expect, it, vi } from 'vitest';

// project imports
import {
  MAX_REFRESH_MS,
  nextRefreshWait,
  readMoreRuns,
  readRunBrowse,
  readSweep,
  REFRESH_MS,
  RunBrowse,
  runBrowseInterval,
  runBrowseKey,
  usesRefresh,
} from './runBrowse';
import { getReaction } from '@thorpi/reactions';
import { listRunsByStatus, listRunsByTag } from '@thorpi/runs';
import { runListKeys } from '@utilities/queries/runs';
import { Reaction, ReactionStatus } from '@models/reactions';

// the listings and the reads by id are answered by each test instead of the API
vi.mock('@thorpi/runs', () => ({ listRunsByStatus: vi.fn(), listRunsByTag: vi.fn() }));
vi.mock('@thorpi/reactions', () => ({ getReaction: vi.fn() }));

/**
 * Build a run as a listing returns it.
 *
 * @param id - The run's id.
 * @param status - The run's status.
 * @returns The run.
 */
function run(id: string, status: ReactionStatus = ReactionStatus.Completed): Reaction {
  return { id, group: 'g', sla: '2026-01-01T00:00:00Z', status } as unknown as Reaction;
}

/**
 * Answer every listing of group `g`: the Completed listing holds the given runs, every other listing is empty.
 *
 * @param completed - The runs of the Completed listing.
 */
function listCompleted(completed: Reaction[]) {
  vi.mocked(listRunsByTag).mockResolvedValue({ runs: [], cursor: null });
  vi.mocked(listRunsByStatus).mockImplementation((_group, status) =>
    Promise.resolve({ runs: status === ReactionStatus.Completed ? completed : [], cursor: null }),
  );
}

beforeEach(() => {
  vi.resetAllMocks();
});

describe('readSweep', () => {
  it('uses the refreshed pages when every listing answers, however many re-reads fail', async () => {
    const completed = run('done');
    listCompleted([completed]);
    // every stale row read again by id answers 500
    vi.mocked(getReaction).mockImplementation((_group, id, onError, onStatus) => {
      onError(`Get Reaction: ${id}`);
      onStatus?.(500);
      return Promise.resolve(null);
    });
    // more stale unfinished rows are shown than there are listings, so their failures outnumber the listings
    const stale = Array.from({ length: 20 }, (_, index) => run(`stale-${index}`, ReactionStatus.Started));
    const shown = [{ ...completed, status: ReactionStatus.Started }, ...stale];
    const errors: string[] = [];
    const sweep = await readSweep(['g'], shown, (message) => errors.push(message));
    expect(errors).toHaveLength(20);
    expect(usesRefresh(sweep)).toBe(true);
    expect(sweep.failures).toBe(0);
    expect(sweep.rereadFailures).toBe(20);
  });
});

describe('nextRefreshWait', () => {
  it('waits the short period after a change and longer after each unchanged reload, up to the cap', () => {
    expect(nextRefreshWait(true, MAX_REFRESH_MS)).toBe(REFRESH_MS);
    expect(nextRefreshWait(false, REFRESH_MS)).toBe(REFRESH_MS * 1.5);
    expect(nextRefreshWait(false, MAX_REFRESH_MS)).toBe(MAX_REFRESH_MS);
  });
});

describe('readRunBrowse', () => {
  it('replaces everything on a full load, even when every listing fails', async () => {
    vi.mocked(listRunsByTag).mockImplementation((_group, _tag, onError) => {
      onError('down');
      return Promise.resolve(null);
    });
    vi.mocked(listRunsByStatus).mockImplementation((_group, _status, onError) => {
      onError('down');
      return Promise.resolve(null);
    });
    const read = await readRunBrowse(['g'], undefined, () => undefined);
    expect(read).toEqual({ runs: [], more: [], error: 'down', signature: '', wait: REFRESH_MS });
  });

  it('merges a refresh into the rows cached when it ends, keeping pages Load more read meanwhile', async () => {
    listCompleted([run('a')]);
    const previous: RunBrowse = { runs: [run('a')], more: [], error: 'old', signature: 'a:Completed', wait: REFRESH_MS };
    // a Load more finished while the refresh ran, adding a row and a cursor
    const latest: RunBrowse = {
      ...previous,
      runs: [run('a'), run('b')],
      more: [{ group: 'g', source: ReactionStatus.Completed, cursor: 9 }],
    };
    const read = await readRunBrowse(['g'], previous, () => latest);
    expect(read.runs.map((entry) => entry.id).sort()).toEqual(['a', 'b']);
    expect(read.more).toEqual(latest.more);
    // the sweep read every listing, so the old error clears; nothing changed, so the next reload waits longer
    expect(read.error).toBe('');
    expect(read.wait).toBe(nextRefreshWait(false, REFRESH_MS));
  });

  it('keeps the rows when every listing of a refresh fails', async () => {
    vi.mocked(listRunsByTag).mockImplementation((_group, _tag, onError) => {
      onError('down');
      return Promise.resolve(null);
    });
    vi.mocked(listRunsByStatus).mockImplementation((_group, _status, onError) => {
      onError('down');
      return Promise.resolve(null);
    });
    const previous: RunBrowse = { runs: [run('a', ReactionStatus.Started)], more: [], error: '', signature: 'a:Started', wait: REFRESH_MS };
    const read = await readRunBrowse(['g'], previous, () => previous);
    expect(read.runs).toEqual(previous.runs);
    expect(read.error).toBe('down');
  });
});

describe('readMoreRuns', () => {
  it('keeps the cursor of a failed page so the next click retries it', async () => {
    vi.mocked(listRunsByStatus).mockImplementation((_group, status, onError) => {
      if (status === ReactionStatus.Failed) {
        onError('page failed');
        return Promise.resolve(null);
      }
      return Promise.resolve({ runs: [run('c')], cursor: 20 });
    });
    const failed = { group: 'g', source: ReactionStatus.Failed, cursor: 5 };
    const completed = { group: 'g', source: ReactionStatus.Completed, cursor: 10 };
    const read = await readMoreRuns([failed, completed]);
    expect(read.lists).toEqual([[run('c')]]);
    expect(read.remaining).toEqual([failed, { ...completed, cursor: 20 }]);
    expect(read.error).toBe('page failed');
  });
});

describe('runBrowseInterval', () => {
  it('reloads only a list with an unfinished run, at its own wait', () => {
    const base: RunBrowse = { runs: [run('a')], more: [], error: '', signature: '', wait: 4500 };
    expect(runBrowseInterval(undefined)).toBe(false);
    expect(runBrowseInterval(base)).toBe(false);
    expect(runBrowseInterval({ ...base, runs: [run('b', ReactionStatus.Started)] })).toBe(4500);
  });
});

describe('runBrowseKey', () => {
  it('sits under the run list key, so a launch makes it stale, and ignores group order', () => {
    expect(runBrowseKey(['b', 'a']).slice(0, runListKeys.runs.length)).toEqual([...runListKeys.runs]);
    expect(runBrowseKey(['b', 'a'])).toEqual(runBrowseKey(['a', 'b']));
  });
});
