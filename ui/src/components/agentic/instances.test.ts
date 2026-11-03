import { describe, expect, it } from 'vitest';

// project imports
import { Severity } from '@components/shared/alerts/AlertBanner';
import { REQUEST_WATCH, requestedNotice, rootIsStale, watchRequestedInstance } from './instances';

const r1 = { id: 'a', name: 'wf#r1' };
const r2 = { id: 'b', name: 'wf#r2' };
const options = { tries: 3, delayMs: 0, sleep: () => Promise.resolve() };

describe('requestedNotice', () => {
  it('does not promise a new instance and names the current one', () => {
    const text = requestedNotice(r1);
    expect(text).toContain('only if the configuration changed since wf#r1');
    expect(text).not.toContain('A new instance was queued');
  });
  it('words the notice without a current instance', () => {
    expect(requestedNotice(undefined)).toContain('only if the configuration changed.');
  });
});

describe('watchRequestedInstance', () => {
  it('reports the instance that appears', async () => {
    let reads = 0;
    const read = () => Promise.resolve(++reads < 2 ? [r1] : [r2, r1]);
    expect(await watchRequestedInstance(read, [r1], options)).toEqual({ text: 'Instance wf#r2 was built.', severity: Severity.Success });
    expect(reads).toBe(2);
  });
  it('does not claim the build was skipped when nothing appears in the window', async () => {
    const { text, severity } = await watchRequestedInstance(() => Promise.resolve([r1]), [r1], options);
    expect(severity).toBe(Severity.Info);
    expect(text).toContain('No new instance has appeared: wf#r1 is the newest');
    expect(text).toContain('the build is still running');
    expect(text).not.toContain('is current');
  });
  it('handles a root that has no instances', async () => {
    expect((await watchRequestedInstance(() => Promise.resolve([]), [], options)).text).toContain('No instance has appeared yet');
  });
  it('finds an instance that a slow build adds after the first reads', async () => {
    let reads = 0;
    // the build lands on the 12th read, as it would 24 s after the request at 2 s per read
    const read = () => Promise.resolve(++reads < 12 ? [r1] : [r2, r1]);
    const text = await watchRequestedInstance(read, [r1], { ...REQUEST_WATCH, sleep: () => Promise.resolve() });
    expect(text).toEqual({ text: 'Instance wf#r2 was built.', severity: Severity.Success });
  });
  it('warns when the new instance is Invalid and names why', async () => {
    const invalid = { id: 'c', name: 'wf#r3', invalid: ['workflow has no entry agent'] };
    const notice = await watchRequestedInstance(() => Promise.resolve([invalid, r1]), [r1], options);
    expect(notice).toEqual({ text: 'Instance wf#r3 was built but is Invalid: workflow has no entry agent.', severity: Severity.Warning });
  });
  it('warns without a reason when an Invalid instance carries no errors', async () => {
    const invalid = { id: 'c', name: 'wf#r3', invalid: [] };
    const notice = await watchRequestedInstance(() => Promise.resolve([invalid, r1]), [r1], options);
    expect(notice).toEqual({ text: 'Instance wf#r3 was built but is Invalid.', severity: Severity.Warning });
  });
  it('watches for at least 25 seconds', () => {
    expect(REQUEST_WATCH.tries * REQUEST_WATCH.delayMs).toBeGreaterThanOrEqual(25_000);
  });
  it('stops reading once cancelled', async () => {
    let reads = 0;
    const read = () => {
      reads += 1;
      return Promise.resolve([r1]);
    };
    await watchRequestedInstance(read, [r1], { ...options, cancelled: () => reads >= 1 });
    expect(reads).toBe(1);
  });
});

describe('rootIsStale', () => {
  it('is stale when an instance has a higher revision than the root records', () => {
    expect(rootIsStale([2, 1], 1)).toBe(true);
    expect(rootIsStale([1], 0)).toBe(true);
  });
  it('is current when the root records the newest revision or there are no instances', () => {
    expect(rootIsStale([2, 1], 2)).toBe(false);
    expect(rootIsStale([], 0)).toBe(false);
  });
});
