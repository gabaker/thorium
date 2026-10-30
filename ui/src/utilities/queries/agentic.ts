import { useQueries, useQuery, useQueryClient } from '@tanstack/react-query';
import { useCallback } from 'react';

// project imports
import { getAgenticEntity, listAllAgentic } from '@thorpi/agentic';
import { AgenticKinds, AnyAgenticEntity } from '@models/agentic';

/** How long an entity list stays fresh; a picker that is opened again within this time reuses the list it read. */
export const AGENTIC_LIST_STALE_MS = 60 * 1000;

/** Query keys for agentic entities, so every page that lists a kind or reads an entity shares one cache entry. */
export const agenticKeys = {
  /** Every agentic list query */
  lists: ['agentic-list'] as const,
  /**
   * The list of one kind.
   *
   * @param kind - The entity kind.
   * @returns The query key.
   */
  list: (kind: AgenticKinds) => ['agentic-list', kind] as const,
  /**
   * One entity.
   *
   * @param id - The entity's id.
   * @returns The query key.
   */
  entity: (id: string) => ['agentic-entity', id] as const,
};

/** A list read for a page: the entities and the message of a failed or cut-off read. */
export interface AgenticListRead<T> {
  /** The entities read; partial when a page failed or the list was cut off */
  items: T[];
  /** Why the list is incomplete, or an empty string */
  error: string;
}

/**
 * Read every entity of one kind the user can see, keeping the message of a failed or cut-off read beside the entities
 * read so far.
 *
 * @template T - The entity type of the kind.
 * @param kind - The kind to list.
 * @returns The entities and the last error message; never throws.
 */
export async function fetchAgenticList<T extends AnyAgenticEntity>(kind: AgenticKinds): Promise<AgenticListRead<T>> {
  let error = '';
  try {
    const items = await listAllAgentic<T>(kind, [], (message) => {
      error = message;
    });
    return { items, error };
  } catch (reason: unknown) {
    // a reply the listing could not read ends the load with an error rather than leaving it loading forever
    return { items: [], error: `List ${kind}: unexpected reply (${reason instanceof Error ? reason.message : String(reason)})` };
  }
}

/** The state of an entity list used by a page. */
export interface AgenticListQueryState<T> extends AgenticListRead<T> {
  /** Whether the first read is in flight */
  loading: boolean;
  /** Read the list again, for every page that shares it */
  reload: () => void;
}

/**
 * Load every entity of one kind the user can see through the query cache.
 *
 * @template T - The entity type of the kind.
 * @param kind - The kind to list.
 * @param enabled - Whether to read the list; a disabled list reads nothing and reports empty, not loading.
 * @param readOnMount - Whether every mount reads the list again even while the cached list is fresh, showing the
 *   cached list until the new one arrives; a page that lists what other pages create or delete wants this.
 * @returns The entities, the error of an incomplete read, whether the first read is in flight, and a reload.
 */
export function useAgenticListQuery<T extends AnyAgenticEntity>(
  kind: AgenticKinds,
  enabled = true,
  readOnMount = false,
): AgenticListQueryState<T> {
  const client = useQueryClient();
  const query = useQuery({
    queryKey: agenticKeys.list(kind),
    queryFn: () => fetchAgenticList<T>(kind),
    staleTime: AGENTIC_LIST_STALE_MS,
    refetchOnMount: readOnMount ? 'always' : true,
    enabled,
  });
  const reload = useCallback(() => void client.invalidateQueries({ queryKey: agenticKeys.list(kind) }), [client, kind]);
  return { items: query.data?.items ?? [], error: query.data?.error ?? '', loading: enabled && query.isPending, reload };
}

/**
 * Read several entities by id through the query cache; an entity that can't be read is left out.
 *
 * @template T - The entity type.
 * @param ids - The ids to read; empty ids are skipped.
 * @returns The entities read so far, by id.
 */
export function useAgenticEntities<T extends AnyAgenticEntity>(ids: string[]): Record<string, T | undefined> {
  const wanted = [...new Set(ids.filter((id) => id !== ''))];
  const queries = useQueries({
    queries: wanted.map((id) => ({
      queryKey: agenticKeys.entity(id),
      queryFn: () => getAgenticEntity<T>(id, () => undefined),
      staleTime: AGENTIC_LIST_STALE_MS,
    })),
  });
  const found: Record<string, T | undefined> = {};
  wanted.forEach((id, index) => {
    found[id] = queries[index].data ?? undefined;
  });
  return found;
}
