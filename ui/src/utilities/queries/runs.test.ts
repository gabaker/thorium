import { QueryClient } from '@tanstack/react-query';
import { beforeEach, describe, expect, it, vi } from 'vitest';

// project imports
import {
  fetchAllStatusUpdates,
  fetchPipelineOrder,
  fetchReaction,
  fetchStageLogs,
  fetchSubRuns,
  isGone,
  MAX_POLL_FAILURES,
  reactionKey,
  RunReadError,
  runRefetchInterval,
  UPDATES_PAGE_SIZE,
} from './runs';
import { getPipeline } from '@thorpi/pipelines';
import { getReaction, getReactionLogs } from '@thorpi/reactions';
import { getRunStageLogs, listSubRuns } from '@thorpi/runs';
import { Reaction, ReactionStatus, StatusUpdate } from '@models/reactions';

vi.mock('@thorpi/reactions', () => ({ getReaction: vi.fn(), getReactionLogs: vi.fn() }));
vi.mock('@thorpi/runs', () => ({ getRunStageLogs: vi.fn(), listSubRuns: vi.fn() }));
vi.mock('@thorpi/pipelines', () => ({ getPipeline: vi.fn() }));

/** A run at a status. */
const run = (status: ReactionStatus) => ({ id: 'r', group: 'g', status }) as Reaction;

/** A query client that never retries, as the pages' queries don't. */
const client = () => new QueryClient({ defaultOptions: { queries: { retry: false } } });

beforeEach(() => vi.resetAllMocks());

describe('fetchReaction', () => {
  it('returns the reaction through a query', async () => {
    vi.mocked(getReaction).mockResolvedValue(run(ReactionStatus.Started));
    await expect(client().fetchQuery({ queryKey: reactionKey('g', 'r'), queryFn: () => fetchReaction('g', 'r') })).resolves.toMatchObject({
      id: 'r',
    });
  });
  it('throws the message and HTTP status of a failed read', async () => {
    vi.mocked(getReaction).mockImplementation((_group, _id, onError, onStatus) => {
      onError('Get Reaction: not found');
      onStatus?.(404);
      return Promise.resolve(null);
    });
    const error = await fetchReaction('g', 'r').catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(RunReadError);
    expect((error as RunReadError).message).toBe('Get Reaction: not found');
    expect(isGone(error)).toBe(true);
  });
});

describe('runRefetchInterval', () => {
  it('polls a working run', () => {
    expect(runRefetchInterval({ data: run(ReactionStatus.Started), error: null }, 0, 2000)).toBe(2000);
    expect(runRefetchInterval({ error: null }, 0, 2000)).toBe(2000);
  });
  it('stops once the run finishes', () => {
    expect(runRefetchInterval({ data: run(ReactionStatus.Completed), error: null }, 0, 2000)).toBe(false);
    expect(runRefetchInterval({ data: run(ReactionStatus.Failed), error: null }, 0, 2000)).toBe(false);
  });
  it('stops at once when the API refuses or lost the run, and after too many failures in a row', () => {
    expect(runRefetchInterval({ error: new RunReadError('gone', 403) }, 1, 2000)).toBe(false);
    expect(runRefetchInterval({ error: new RunReadError('down', 503) }, 1, 2000)).toBe(2000);
    expect(runRefetchInterval({ error: new RunReadError('down', 503) }, MAX_POLL_FAILURES, 2000)).toBe(false);
  });
});

describe('fetchAllStatusUpdates', () => {
  /** A page of updates of a size. */
  const page = (size: number) => Array.from({ length: size }, () => ({ action: 'JobCreated' }) as StatusUpdate);
  it('reads page after page by offset until a short page', async () => {
    vi.mocked(getReactionLogs).mockResolvedValueOnce(page(UPDATES_PAGE_SIZE)).mockResolvedValueOnce(page(3));
    await expect(fetchAllStatusUpdates('g', 'r')).resolves.toHaveLength(UPDATES_PAGE_SIZE + 3);
    expect(vi.mocked(getReactionLogs).mock.calls.map((call) => call[3])).toEqual([null, String(UPDATES_PAGE_SIZE)]);
  });
  it('treats an answer that is not a list as no updates', async () => {
    vi.mocked(getReactionLogs).mockResolvedValue({} as unknown as StatusUpdate[]);
    await expect(fetchAllStatusUpdates('g', 'r')).resolves.toEqual([]);
  });
  it('throws when a read fails, so the query keeps the updates it had', async () => {
    vi.mocked(getReactionLogs).mockImplementation((_group, _id, onError) => {
      onError('Get Reaction Logs: down');
      return Promise.resolve(null);
    });
    await expect(fetchAllStatusUpdates('g', 'r')).rejects.toThrow('Get Reaction Logs: down');
  });
});

describe('the other run reads', () => {
  it('reads stage logs and throws on a failure', async () => {
    vi.mocked(getRunStageLogs).mockResolvedValueOnce(['a']);
    await expect(fetchStageLogs('g', 'r', 'scan')).resolves.toEqual(['a']);
    vi.mocked(getRunStageLogs).mockResolvedValueOnce(null);
    await expect(fetchStageLogs('g', 'r', 'scan')).rejects.toBeInstanceOf(RunReadError);
  });
  it('keeps the sub reactions read before a failure, and throws when none were read', async () => {
    vi.mocked(listSubRuns).mockImplementationOnce((_group, _id, onError) => {
      onError('page 2 failed');
      return Promise.resolve([run(ReactionStatus.Completed)]);
    });
    await expect(fetchSubRuns('g', 'r')).resolves.toHaveLength(1);
    vi.mocked(listSubRuns).mockImplementationOnce((_group, _id, onError) => {
      onError('down');
      return Promise.resolve([]);
    });
    await expect(fetchSubRuns('g', 'r')).rejects.toThrow('down');
  });
  it('reads a pipeline order with single image stages as lists', async () => {
    vi.mocked(getPipeline).mockResolvedValue({ order: ['scan', ['a', 'b']] } as never);
    await expect(fetchPipelineOrder('g', 'p')).resolves.toEqual([['scan'], ['a', 'b']]);
  });
});
