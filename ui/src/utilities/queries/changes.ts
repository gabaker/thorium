//! Changes that keep the query cache true: each runs a change against the API, then marks stale the cached reads the
//! change can alter, so a page never shows what it read before the change for the rest of the read's fresh time.
import { QueryClient, useQueryClient } from '@tanstack/react-query';

/** Mark stale the cached reads of one kind of data, returning once the reads in use have been made again. */
export type Invalidate = (client: QueryClient) => Promise<void>;

/**
 * Run a change, then mark stale the cached reads it can alter.
 *
 * The reads are invalidated whether the change succeeds, is refused, or throws, since a refused change (such as a
 * delete that stops part way) may have applied in part. The change's result is returned without waiting for the reads
 * in use to be made again, so a page that navigates away after a change is not held up by reads it no longer needs.
 *
 * @template R - What the change resolves to.
 * @param client - The query client.
 * @param invalidate - The invalidation, or invalidations, for the data the change alters.
 * @param change - The change to run.
 * @returns What the change resolved to.
 */
export async function changeAndInvalidate<R>(
  client: QueryClient,
  invalidate: Invalidate | Invalidate[],
  change: () => Promise<R>,
): Promise<R> {
  try {
    return await change();
  } finally {
    // invalidating marks every matching read stale at once; only the reread of the reads in use is left running
    for (const run of Array.isArray(invalidate) ? invalidate : [invalidate]) void run(client);
  }
}

/**
 * Get a runner for a page's changes that marks stale the cached reads they alter, as {@link changeAndInvalidate} does.
 *
 * @param invalidate - The invalidation, or invalidations, for the data the page's changes alter.
 * @returns A function that runs a change and resolves to what it resolved to.
 */
export function useChangeAndInvalidate(invalidate: Invalidate | Invalidate[]): <R>(change: () => Promise<R>) => Promise<R> {
  const client = useQueryClient();
  return (change) => changeAndInvalidate(client, invalidate, change);
}
