import { QueryClient } from '@tanstack/react-query';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it, vi } from 'vitest';

// project imports
import { agenticKeys, fetchAgenticEntity, useAgenticEntities, useAgenticEntityRead, useAgenticEntityReads } from './agentic';
import { createTestQueryClient, withQueryClient } from '@utilities/testing/queryClient';
import { AnyAgenticEntity } from '@models/agentic';

// a read the API refuses gives no message of its own here, so the fallback message is used
vi.mock('@thorpi/agentic', () => ({ getAgenticEntity: () => Promise.resolve(null), listAllAgentic: vi.fn() }));

/**
 * Render what both entity hooks give for some ids, as JSON.
 *
 * @param props - The probe's props.
 * @param props.ids - The ids to read.
 * @returns The reads and the entities, as JSON.
 */
const Probe = ({ ids }: { ids: string[] }) => {
  const reads = useAgenticEntityReads<AnyAgenticEntity>(ids);
  const entities = useAgenticEntities<AnyAgenticEntity>(ids);
  // JSON drops undefined values, so each read is named by what it holds
  const describe = (value: unknown) => (value === undefined ? 'reading' : value === null ? 'unreadable' : 'read');
  return (
    <>
      {JSON.stringify({
        reads: Object.fromEntries(Object.entries(reads).map(([id, value]) => [id, describe(value)])),
        entities: Object.fromEntries(Object.entries(entities).map(([id, value]) => [id, describe(value)])),
      })}
    </>
  );
};

describe('agentic entity reads', () => {
  it('tells a read entity, an unreadable one, and one still being read apart, and the plain hook leaves out failures', async () => {
    const client = createTestQueryClient();
    client.setQueryData(agenticKeys.entity('a'), { id: 'a' });
    await client.prefetchQuery({ queryKey: agenticKeys.entity('b'), queryFn: () => Promise.reject(new Error('missing')) });
    const markup = renderToStaticMarkup(withQueryClient(<Probe ids={['a', 'b', 'c', '']} />, client)).replaceAll('&quot;', '"');
    expect(JSON.parse(markup)).toEqual({
      reads: { a: 'read', b: 'unreadable', c: 'reading' },
      entities: { a: 'read', b: 'reading', c: 'reading' },
    });
  });
});

/**
 * Render what the single entity hook gives for an id, as JSON.
 *
 * @param props - The probe's props.
 * @param props.id - The id to read.
 * @returns The read, as JSON.
 */
const OneProbe = ({ id }: { id: string }) => {
  const { entity, error } = useAgenticEntityRead<AnyAgenticEntity>(id);
  return <>{JSON.stringify({ entity: entity === undefined ? 'reading' : entity === null ? 'unreadable' : entity.id, error })}</>;
};

/**
 * Render the single entity probe.
 *
 * @param client - The query client holding the cached reads.
 * @param id - The id to read.
 * @returns The probe's JSON, parsed.
 */
const renderOne = (client: QueryClient, id: string) =>
  JSON.parse(renderToStaticMarkup(withQueryClient(<OneProbe id={id} />, client)).replaceAll('&quot;', '"')) as unknown;

describe('agentic entity read', () => {
  it('keeps an entity read before when a later reread fails', async () => {
    const client = createTestQueryClient();
    client.setQueryData(agenticKeys.entity('a'), { id: 'a' });
    await client
      .fetchQuery({ queryKey: agenticKeys.entity('a'), queryFn: () => Promise.reject(new Error('gone')), staleTime: 0 })
      .catch(() => undefined);
    expect(renderOne(client, 'a')).toEqual({ entity: 'a', error: '' });
  });

  it('gives the message of a read that failed and reads nothing for an empty id', async () => {
    const client = createTestQueryClient();
    await expect(fetchAgenticEntity('missing')).rejects.toThrow('Get Entity: missing could not be read');
    expect(renderOne(client, '')).toEqual({ entity: 'reading', error: '' });
  });
});
