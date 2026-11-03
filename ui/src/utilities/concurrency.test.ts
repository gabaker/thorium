import { describe, expect, it } from 'vitest';

// project imports
import { mapWithConcurrency } from './concurrency';

/** Wait for a number of milliseconds. */
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

describe('mapWithConcurrency', () => {
  it('returns the results in item order whatever order the calls finish in', async () => {
    const results = await mapWithConcurrency([30, 10, 20], 3, async (ms, index) => {
      await sleep(ms);
      return `${index}:${ms}`;
    });
    expect(results).toEqual(['0:30', '1:10', '2:20']);
  });

  it('never has more than the limit of calls in flight', async () => {
    let inFlight = 0;
    let most = 0;
    await mapWithConcurrency(
      Array.from({ length: 12 }, (_, index) => index),
      4,
      async (index) => {
        inFlight += 1;
        most = Math.max(most, inFlight);
        await sleep(index % 3);
        inFlight -= 1;
      },
    );
    expect(most).toBe(4);
  });

  it('runs every item one at a time with a limit below one, and nothing for no items', async () => {
    const order: number[] = [];
    await mapWithConcurrency([1, 2, 3], 0, (item) => {
      order.push(item);
      return Promise.resolve();
    });
    expect(order).toEqual([1, 2, 3]);
    expect(await mapWithConcurrency([], 4, () => Promise.resolve(1))).toEqual([]);
  });

  it('rejects with the first error at once, and still starts the remaining items unless the limit is 1', async () => {
    const started: number[] = [];
    // item 1 fails at once; the others take a little while
    const run = async (item: number) => {
      started.push(item);
      if (item === 1) throw new Error('boom');
      await sleep(5);
      return item;
    };
    await expect(mapWithConcurrency([1, 2, 3, 4], 2, run)).rejects.toThrow('boom');
    await sleep(30);
    expect([...started].sort()).toEqual([1, 2, 3, 4]);
    started.length = 0;
    await expect(mapWithConcurrency([1, 2, 3], 1, run)).rejects.toThrow('boom');
    await sleep(30);
    expect(started).toEqual([1]);
  });
});
