import { afterEach, describe, expect, it, vi } from 'vitest';

// project imports
import client from './client';
import { getReactionLogs, getReactionStageLogs, listReactions } from './reactions';

afterEach(() => {
  vi.restoreAllMocks();
});

describe('reaction path segments', () => {
  it('encodes the stage name of a stage log request', async () => {
    const get = vi.spyOn(client, 'get').mockResolvedValue({ status: 200, data: { logs: ['one'] } });
    await expect(getReactionStageLogs('g', 'id', 'tool@1#a?b', () => undefined)).resolves.toEqual(['one']);
    expect(get).toHaveBeenCalledWith('/reactions/logs/g/id/tool%401%23a%3Fb', { params: { limit: 100 } });
  });

  it('encodes the group and id of a reaction log request', async () => {
    const get = vi.spyOn(client, 'get').mockResolvedValue({ status: 200, data: [] });
    await getReactionLogs('g/x', 'id#1', () => undefined);
    expect(get).toHaveBeenCalledWith('/reactions/logs/g%2Fx/id%231', { params: { limit: 100 } });
  });

  it('encodes the tag of a tag listing', async () => {
    const get = vi.spyOn(client, 'get').mockResolvedValue({ status: 200, data: { details: [] } });
    await listReactions('g', () => undefined, '', 'a#b', true);
    expect(get.mock.calls[0][0]).toContain('tag/g/a%23b/details/');
  });

  it('encodes the pipeline of a pipeline listing', async () => {
    const get = vi.spyOn(client, 'get').mockResolvedValue({ status: 200, data: { names: [] } });
    await listReactions('g', () => undefined, 'pipe?x');
    expect(get.mock.calls[0][0]).toBe('/reactions/list/g/pipe%3Fx/');
  });
});
