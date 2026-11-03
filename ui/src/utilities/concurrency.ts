/**
 * Run an async function over every item with at most `limit` calls in flight at once.
 *
 * The calls start in item order, a new one starting as soon as an earlier one settles, so a long list overlaps its
 * requests' latency without sending them all at once.
 *
 * @template T - The type of the items.
 * @template R - The type each call resolves to.
 * @param items - The items to run the function over.
 * @param limit - The most calls in flight at once; below 1 counts as 1.
 * @param run - The function to call with each item and its index.
 * @returns What each call resolved to, in item order.
 * @throws The first error a call throws, as soon as it is thrown. The calls in flight still settle and the items not yet
 *   started are still started, their results dropped; only with a limit of 1 do the remaining items never start.
 */
export async function mapWithConcurrency<T, R>(items: T[], limit: number, run: (item: T, index: number) => Promise<R>): Promise<R[]> {
  const results = new Array<R>(items.length);
  let next = 0;
  // each worker claims the next unstarted item until none are left; JavaScript runs one worker at a time between awaits
  const worker = async (): Promise<void> => {
    while (next < items.length) {
      const index = next++;
      results[index] = await run(items[index], index);
    }
  };
  await Promise.all(Array.from({ length: Math.min(Math.max(1, limit), items.length) }, () => worker()));
  return results;
}
