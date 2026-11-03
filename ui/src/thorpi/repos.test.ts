import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { MockInstance } from 'vitest';

// project imports
import client, { parseRequestError } from './client';
import { listCommitishDetails } from './repos';

describe('listCommitishDetails', () => {
  let get: MockInstance;
  beforeEach(() => {
    get = vi.spyOn(client, 'get').mockResolvedValue({ status: 200, data: { data: [], cursor: null } });
  });

  it('reads the first page with only the limit', async () => {
    await listCommitishDetails('git.example.test/org/name', () => undefined, 50);
    expect(get.mock.lastCall?.[1]).toEqual({ params: { limit: 50 } });
  });

  it('sends the cursor of the page to continue from', async () => {
    await listCommitishDetails('git.example.test/org/name', () => undefined, 100, 'cursor-id');
    expect(get.mock.lastCall?.[1]).toEqual({ params: { limit: 100, cursor: 'cursor-id' } });
  });

  it('hands a failed request to the error handler and returns null', async () => {
    get.mockRejectedValue(new Error('boom'));
    const page = await listCommitishDetails('git.example.test/org/name', () => undefined);
    expect(page).toBeNull();
    expect(parseRequestError).toHaveBeenCalledWith(expect.any(Error), expect.any(Function), 'List Commitishes');
  });
});
