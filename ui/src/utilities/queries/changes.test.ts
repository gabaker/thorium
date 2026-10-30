import { QueryClient, QueryKey } from '@tanstack/react-query';
import { describe, expect, it, vi } from 'vitest';

// project imports
import { agenticKeys, AGENTIC_LIST_STALE_MS, invalidateAgentic } from './agentic';
import { changeAndInvalidate } from './changes';
import { groupKeys, GROUP_STALE_MS, invalidateGroupAccess, invalidateGroups } from './groups';
import { invalidatePipelines, pipelineKeys, PIPELINE_LIST_STALE_MS } from './pipelines';
import { invalidateRunLists, pipelineOrderKey, runListKeys } from './runs';
import { AgenticKinds } from '@models/agentic';

vi.mock('@thorpi/agentic', () => ({ getAgenticEntity: vi.fn(), listAllAgentic: vi.fn() }));
vi.mock('@thorpi/groups', () => ({ getGroup: vi.fn() }));
vi.mock('@thorpi/pipelines', () => ({ getPipeline: vi.fn(), listPipelines: vi.fn() }));

/** A query client that never retries, as the pages' queries don't. */
const client = () => new QueryClient({ defaultOptions: { queries: { retry: false } } });

/** What the server holds for a key: the value a read returns, which a test changes as the server would. */
interface Server {
  /** The value the next read returns */
  value: string;
}

/**
 * Read a key the way the pages do: through the cache, reusing a cached value while it is fresh.
 *
 * @param queries - The query client.
 * @param queryKey - The key to read.
 * @param server - What the server holds.
 * @param staleTime - How long a read stays fresh.
 * @returns The value the page would show.
 */
const read = (queries: QueryClient, queryKey: QueryKey, server: Server, staleTime: number) =>
  queries.fetchQuery({ queryKey, queryFn: () => Promise.resolve(server.value), staleTime });

/** Each cached read a change can alter, with the invalidation its changes run and the time the read stays fresh. */
const cases: { name: string; key: QueryKey; invalidate: (queries: QueryClient) => Promise<void>; staleTime: number }[] = [
  { name: 'an agentic list', key: agenticKeys.list(AgenticKinds.Tool), invalidate: invalidateAgentic, staleTime: AGENTIC_LIST_STALE_MS },
  { name: 'an agentic entity', key: agenticKeys.entity('t1'), invalidate: invalidateAgentic, staleTime: AGENTIC_LIST_STALE_MS },
  { name: 'a pipeline list', key: pipelineKeys.details(['g']), invalidate: invalidatePipelines, staleTime: PIPELINE_LIST_STALE_MS },
  { name: "a run's pipeline order", key: pipelineOrderKey('g', 'p'), invalidate: invalidatePipelines, staleTime: Infinity },
  { name: "a group's details", key: groupKeys.details('g'), invalidate: invalidateGroups, staleTime: GROUP_STALE_MS },
  { name: 'a run list', key: [...runListKeys.runs, 'browse', 'g'], invalidate: invalidateRunLists, staleTime: Infinity },
];

describe.each(cases)('changing what $name shows', ({ key, invalidate, staleTime }) => {
  it('shows the old value when the change does not invalidate the cache', async () => {
    const queries = client();
    const server = { value: 'before' };
    await read(queries, key, server, staleTime);
    server.value = 'after';
    // the cached read is still fresh, so the page shows what it held before the change
    await expect(read(queries, key, server, staleTime)).resolves.toBe('before');
  });

  it('shows the new value after a change made through changeAndInvalidate', async () => {
    const queries = client();
    const server = { value: 'before' };
    await read(queries, key, server, staleTime);
    const result = await changeAndInvalidate(queries, invalidate, () => {
      server.value = 'after';
      return Promise.resolve(true);
    });
    expect(result).toBe(true);
    await expect(read(queries, key, server, staleTime)).resolves.toBe('after');
  });
});

describe('changeAndInvalidate', () => {
  it('invalidates even when the change fails, since a failed change may have applied in part', async () => {
    const queries = client();
    const server = { value: 'before' };
    await read(queries, agenticKeys.entity('t1'), server, AGENTIC_LIST_STALE_MS);
    await expect(
      changeAndInvalidate(queries, invalidateAgentic, () => {
        server.value = 'after';
        return Promise.reject(new Error('refused'));
      }),
    ).rejects.toThrow('refused');
    await expect(read(queries, agenticKeys.entity('t1'), server, AGENTIC_LIST_STALE_MS)).resolves.toBe('after');
  });

  it('runs every invalidation it is given', async () => {
    const queries = client();
    const server = { value: 'before' };
    await read(queries, groupKeys.details('g'), server, GROUP_STALE_MS);
    await read(queries, pipelineKeys.details(['g']), server, PIPELINE_LIST_STALE_MS);
    server.value = 'after';
    await changeAndInvalidate(queries, [invalidateGroups, invalidatePipelines], () => Promise.resolve(null));
    await expect(read(queries, groupKeys.details('g'), server, GROUP_STALE_MS)).resolves.toBe('after');
    await expect(read(queries, pipelineKeys.details(['g']), server, PIPELINE_LIST_STALE_MS)).resolves.toBe('after');
  });

  it('marks the agentic and pipeline lists stale after leaving or deleting a group', async () => {
    const queries = client();
    const server = { value: 'before' };
    await read(queries, groupKeys.details('g'), server, GROUP_STALE_MS);
    await read(queries, agenticKeys.list(AgenticKinds.Tool), server, AGENTIC_LIST_STALE_MS);
    await read(queries, pipelineKeys.details(['g']), server, PIPELINE_LIST_STALE_MS);
    server.value = 'after';
    // the user may lose sight of what the group held, so every list drawn from its groups is read again
    await changeAndInvalidate(queries, invalidateGroupAccess, () => Promise.resolve(true));
    await expect(read(queries, groupKeys.details('g'), server, GROUP_STALE_MS)).resolves.toBe('after');
    await expect(read(queries, agenticKeys.list(AgenticKinds.Tool), server, AGENTIC_LIST_STALE_MS)).resolves.toBe('after');
    await expect(read(queries, pipelineKeys.details(['g']), server, PIPELINE_LIST_STALE_MS)).resolves.toBe('after');
  });

  it('leaves reads of other kinds cached', async () => {
    const queries = client();
    const server = { value: 'before' };
    await read(queries, groupKeys.details('g'), server, GROUP_STALE_MS);
    server.value = 'after';
    await changeAndInvalidate(queries, invalidateAgentic, () => Promise.resolve(true));
    await expect(read(queries, groupKeys.details('g'), server, GROUP_STALE_MS)).resolves.toBe('before');
  });
});
