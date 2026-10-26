import { describe, expect, it, vi } from 'vitest';

// project imports
import client from './client';
import { cancelRun, getRunStageLogs, listRunsByStatus, listRunsByTag, listSubRuns } from './runs';
import { ReactionStatus } from '@models/reactions';

describe('cancelRun', () => {
  it('returns the status the handle route accepts the command with', async () => {
    // the handle route answers a successful command with 202 Accepted
    vi.spyOn(client, 'post').mockResolvedValue({ status: 202, data: { status: ReactionStatus.Failed } });
    const errors: string[] = [];
    await expect(cancelRun('group', 'id', (error) => errors.push(error))).resolves.toBe(ReactionStatus.Failed);
    expect(errors).toEqual([]);
  });

  it('returns null when the request fails', async () => {
    vi.spyOn(client, 'post').mockRejectedValue(new Error('down'));
    await expect(cancelRun('group', 'id', () => undefined)).resolves.toBeNull();
  });
});

describe('getRunStageLogs', () => {
  it('returns the log lines of a stage', async () => {
    vi.spyOn(client, 'get').mockResolvedValue({ status: 200, data: { logs: ['one'] } });
    await expect(getRunStageLogs('group', 'id', 'scan', () => undefined)).resolves.toEqual(['one']);
  });

  it('returns null, not an empty list, when the request fails', async () => {
    vi.spyOn(client, 'get').mockRejectedValue(new Error('down'));
    await expect(getRunStageLogs('group', 'id', 'scan', () => undefined)).resolves.toBeNull();
  });
});

describe('run listings', () => {
  it('returns the cursor of the next page and sends the one it is given', async () => {
    const get = vi.spyOn(client, 'get').mockResolvedValue({ status: 200, data: { details: [{ id: 'a' }], cursor: 50 } });
    await expect(listRunsByStatus('g', ReactionStatus.Completed, () => undefined, 10)).resolves.toEqual({
      runs: [{ id: 'a' }],
      cursor: 50,
    });
    expect(get).toHaveBeenCalledWith('/reactions/group/g/Completed/details/', { params: { limit: 50, cursor: 10 } });
    await listRunsByTag('g', 'WorkflowRun', () => undefined);
    expect(get).toHaveBeenLastCalledWith('/reactions/tag/g/WorkflowRun/details/', { params: { limit: 50 } });
  });

  it('reports the last page with a null cursor and a failure as null', async () => {
    vi.spyOn(client, 'get').mockResolvedValueOnce({ status: 200, data: { details: [] } });
    await expect(listRunsByTag('g', 'WorkflowRun', () => undefined)).resolves.toEqual({ runs: [], cursor: null });
    vi.spyOn(client, 'get').mockRejectedValueOnce(new Error('down'));
    await expect(listRunsByTag('g', 'WorkflowRun', () => undefined)).resolves.toBeNull();
  });

  it('follows the cursor through the sub reactions of a run', async () => {
    const get = vi
      .spyOn(client, 'get')
      .mockResolvedValueOnce({ status: 200, data: { details: [{ id: 'a' }], cursor: 7 } })
      .mockResolvedValueOnce({ status: 200, data: { details: [{ id: 'b' }] } });
    await expect(listSubRuns('g', 'r', () => undefined)).resolves.toEqual([{ id: 'a' }, { id: 'b' }]);
    expect(get).toHaveBeenLastCalledWith('/reactions/sub/g/r/details/', { params: { limit: 50, cursor: 7 } });
  });
});
