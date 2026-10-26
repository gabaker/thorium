import { describe, expect, it, Mock, vi } from 'vitest';

// project imports
import client from './client';
import { listAllAgentic, requestInstance } from './agentic';
import { AgenticKinds, AnyAgenticEntity } from '@models/agentic';

describe('listAllAgentic', () => {
  it('reads pages until the cursor ends', async () => {
    // the client is the stub axios instance vitest.setup.ts provides
    const get = client['get'] as Mock;
    get.mockReset();
    get
      .mockResolvedValueOnce({ status: 200, data: { data: [{ id: 'a' }], cursor: 'c1' } })
      .mockResolvedValueOnce({ status: 200, data: { data: [{ id: 'b' }] } });
    const errors: string[] = [];
    const all = await listAllAgentic<AnyAgenticEntity>(AgenticKinds.Tool, [], (error) => errors.push(error));
    expect(all.map((entity) => entity.id)).toEqual(['a', 'b']);
    expect(errors).toEqual([]);
  });
  it('reports a listing cut off by the page cap', async () => {
    // the client is the stub axios instance vitest.setup.ts provides
    const get = client['get'] as Mock;
    get.mockReset();
    // every page names a next page, so the listing outgrows the cap
    get.mockResolvedValue({ status: 200, data: { data: [{ id: 'x' }], cursor: 'more' } });
    const errors: string[] = [];
    const all = await listAllAgentic<AnyAgenticEntity>(AgenticKinds.Tool, [], (error) => errors.push(error), 3);
    expect(all).toHaveLength(3);
    expect(get).toHaveBeenCalledTimes(3);
    expect(errors).toHaveLength(1);
    expect(errors[0]).toMatch(/Tool.*3/);
  });
});

describe('requestInstance', () => {
  it('encodes the root id into the path, so a crafted id cannot reach another route', async () => {
    const post = vi.spyOn(client, 'post').mockResolvedValue({ status: 202 });
    await expect(requestInstance('workflows', '../../users/delete-me', () => undefined)).resolves.toBe(true);
    expect(post).toHaveBeenCalledWith('/entities/workflows/..%2F..%2Fusers%2Fdelete-me/instance');
  });
});
