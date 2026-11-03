import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

// project imports
import { sampleChecks, sampleKeys, sampleLookupTargets, useSampleChecks } from './samples';
import { createTestQueryClient, withQueryClient } from '@utilities/testing/queryClient';
import { HashCheck, HashCheckStatus } from '@utilities/hashes';

/** Two well-formed SHA256s. */
const A = 'a'.repeat(64);
const B = 'b'.repeat(64);

/** A sample as the API serves it. */
const SAMPLE = {
  sha256: A,
  sha1: '',
  md5: '',
  tags: {},
  comments: [],
  submissions: [{ id: 's', name: 'evil.exe', description: null, groups: ['g'] }],
};

describe('sampleChecks', () => {
  it('marks a found sample valid with its submitted name', () => {
    expect(sampleChecks([A], [A], { [A]: { done: true, sample: SAMPLE as never } })[A]).toEqual({
      status: HashCheckStatus.Valid,
      name: 'evil.exe',
    });
  });
  it('marks a sample the user cannot see as not found or not accessible', () => {
    expect(sampleChecks([A], [A], { [A]: { done: true, sample: null } })[A]).toMatchObject({
      status: HashCheckStatus.Missing,
      reason: 'not found or not accessible',
    });
  });
  it('fails a malformed entry at once, with no lookup', () => {
    expect(sampleChecks(['nothex'], [], {}).nothex.status).toBe(HashCheckStatus.Malformed);
    expect(sampleLookupTargets(['nothex'], ['nothex'])).toEqual([]);
  });
  it('keeps a hash pending until typing has paused and its lookup answered', () => {
    // the debounce: B was entered after the last pause, so it isn't looked up yet
    expect(sampleLookupTargets([A, B], [A])).toEqual([A]);
    expect(sampleChecks([A, B], [A], { [A]: { done: false, sample: undefined } })).toEqual({
      [A]: { status: HashCheckStatus.Pending },
      [B]: { status: HashCheckStatus.Pending },
    });
  });
  it('keys each sample once, shared with other readers', () => {
    expect(sampleKeys.detail(A)).toEqual(['sample', A]);
  });
});

/** Render the checks of some hashes as JSON. */
const Probe = ({ hashes }: { hashes: string[] }) => <pre>{JSON.stringify(useSampleChecks(hashes))}</pre>;

describe('useSampleChecks', () => {
  it('reads a cached sample from the shared query cache', () => {
    const client = createTestQueryClient();
    client.setQueryData(sampleKeys.detail(A), SAMPLE);
    client.setQueryData(sampleKeys.detail(B), null);
    const markup = renderToStaticMarkup(withQueryClient(<Probe hashes={[A, B, 'nothex']} />, client)).replaceAll('&quot;', '"');
    const checks = JSON.parse(markup.replace(/^<pre>|<\/pre>$/g, '')) as Record<string, HashCheck>;
    expect(checks[A].status).toBe(HashCheckStatus.Valid);
    expect(checks[B].status).toBe(HashCheckStatus.Missing);
    expect(checks.nothex.status).toBe(HashCheckStatus.Malformed);
  });
});
