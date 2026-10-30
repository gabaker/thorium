import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { agenticKeys, useAgenticEntities, useAgenticEntityReads } from './agentic';
import { createTestQueryClient, withQueryClient } from '@utilities/testing/queryClient';
import { AnyAgenticEntity } from '@models/agentic';

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
  it('tells a read entity, an unreadable one, and one still being read apart, and the plain hook leaves out failures', () => {
    const client = createTestQueryClient();
    client.setQueryData(agenticKeys.entity('a'), { id: 'a' });
    client.setQueryData(agenticKeys.entity('b'), null);
    const markup = renderToStaticMarkup(withQueryClient(<Probe ids={['a', 'b', 'c', '']} />, client)).replaceAll('&quot;', '"');
    expect(JSON.parse(markup)).toEqual({
      reads: { a: 'read', b: 'unreadable', c: 'reading' },
      entities: { a: 'read', b: 'reading', c: 'reading' },
    });
  });
});
