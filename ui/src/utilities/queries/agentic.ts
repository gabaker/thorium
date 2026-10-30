import { QueryClient, useQueries, useQuery, useQueryClient } from '@tanstack/react-query';
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
  /** Every query that reads one entity, such as a tool with its versions */
  entities: ['agentic-entity'] as const,
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

/**
 * Mark every cached agentic list and entity stale, for a change to any tool, agent, toolset, workflow, model, or
 * instance; one change can alter several (a new version changes its tool, and a deleted tool every list that held it).
 *
 * @param client - The query client.
 * @returns A promise that settles once the lists and entities in use have been read again.
 */
export async function invalidateAgentic(client: QueryClient): Promise<void> {
  await Promise.all([
    client.invalidateQueries({ queryKey: agenticKeys.lists }),
    client.invalidateQueries({ queryKey: agenticKeys.entities }),
  ]);
}

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

/** A failed read of one entity: the formatted message and the HTTP status (0 when the server gave no response). */
export class AgenticReadError extends Error {
  /** The HTTP status of the failed request, or 0 when the server gave no response */
  status: number;

  /**
   * Describe a failed entity read.
   *
   * @param message - The formatted message of the failure.
   * @param status - The HTTP status of the failed request.
   */
  constructor(message: string, status: number) {
    super(message);
    this.name = 'AgenticReadError';
    this.status = status;
  }
}

/**
 * Read one entity by id, throwing when it can't be read so a query keeps the entity it read before.
 *
 * @template T - The entity type.
 * @param id - The entity's id.
 * @returns The entity.
 * @throws {AgenticReadError} When the entity is missing, hidden, or the read failed.
 */
export async function fetchAgenticEntity<T extends AnyAgenticEntity>(id: string): Promise<T> {
  let message = '';
  let status = 0;
  const found = await getAgenticEntity<T>(
    id,
    (error) => (message = error),
    (code) => (status = code),
  );
  if (!found) throw new AgenticReadError(message || `Get Entity: ${id} could not be read`, status);
  return found;
}

/**
 * Get the query options that read one entity through the shared entity cache.
 *
 * A failed read is not retried: a missing or hidden entity stays so, and the page offers its own retry. A failed reread
 * keeps the entity read before, so an entity deleted while it is shown doesn't flash an error before the page moves on.
 *
 * @template T - The entity type.
 * @param id - The entity's id.
 * @returns The query options.
 */
export function agenticEntityQuery<T extends AnyAgenticEntity>(id: string) {
  return { queryKey: agenticKeys.entity(id), queryFn: () => fetchAgenticEntity<T>(id), staleTime: AGENTIC_LIST_STALE_MS, retry: false };
}

/** One entity read through the cache, with why it can't be read. */
export interface AgenticEntityRead<T> {
  /** The entity, `null` when it can't be read, or `undefined` while it is read (or no id was given) */
  entity: T | null | undefined;
  /** Why the entity can't be read, or an empty string */
  error: string;
}

/**
 * Read one entity by id through the query cache; an entity that couldn't be read is read again when it is asked for again.
 *
 * @template T - The entity type.
 * @param id - The entity's id; an empty id reads nothing.
 * @returns The entity and the message of a read that never succeeded.
 */
export function useAgenticEntityRead<T extends AnyAgenticEntity>(id: string): AgenticEntityRead<T> {
  const query = useQuery({ ...agenticEntityQuery<T>(id), enabled: id !== '' });
  // an entity read before stays shown when a later reread fails
  if (query.data !== undefined) return { entity: query.data, error: '' };
  return query.isError ? { entity: null, error: query.error.message } : { entity: undefined, error: '' };
}

/**
 * Read several entities by id through the query cache, all at once, telling an entity still being read from one that
 * can't be read.
 *
 * @template T - The entity type.
 * @param ids - The ids to read; empty ids are skipped.
 * @returns Each entity by id: the entity, `null` when it can't be read, or `undefined` while it is read.
 */
export function useAgenticEntityReads<T extends AnyAgenticEntity>(ids: string[]): Record<string, T | null | undefined> {
  const wanted = [...new Set(ids.filter((id) => id !== ''))];
  // an entity that can't be read is not read again on every mount, only once it goes stale or a change invalidates it
  const queries = useQueries({ queries: wanted.map((id) => ({ ...agenticEntityQuery<T>(id), retryOnMount: false })) });
  const found: Record<string, T | null | undefined> = {};
  wanted.forEach((id, index) => {
    const query = queries[index];
    // an entity read before stays when a later reread fails
    found[id] = query.data ?? (query.isError ? null : undefined);
  });
  return found;
}

/**
 * Read several entities by id through the query cache; an entity that can't be read is left out.
 *
 * @template T - The entity type.
 * @param ids - The ids to read; empty ids are skipped.
 * @returns The entities read so far, by id.
 */
export function useAgenticEntities<T extends AnyAgenticEntity>(ids: string[]): Record<string, T | undefined> {
  const reads = useAgenticEntityReads<T>(ids);
  return Object.fromEntries(Object.entries(reads).map(([id, entity]) => [id, entity ?? undefined]));
}
