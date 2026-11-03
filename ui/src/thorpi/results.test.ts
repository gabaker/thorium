import axios from 'axios';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { MockInstance } from 'vitest';

// project imports
import client, { parseRequestError } from './client';
import { getRepoResults, getResultEntities, getResultsFile } from './results';

/** A workflow run's result key; the `#` must not be read as a URL fragment. */
const KEY = 'triage#r3~0d9c57aa-0000-0000-0000-000000000000';

describe('result file paths', () => {
  let get: MockInstance;
  beforeEach(() => {
    get = vi.spyOn(client, 'get').mockResolvedValue({ status: 200, data: [] });
  });

  it('encodes a workflow result key in a result file download', async () => {
    await getResultsFile('a'.repeat(64), KEY, 'result-id', 'report.txt', () => undefined);
    const url = get.mock.lastCall?.[0] as string;
    expect(url).toBe(`/files/result-files/${'a'.repeat(64)}/triage%23r3~0d9c57aa-0000-0000-0000-000000000000/result-id`);
  });

  it('encodes a workflow result key in a result entities download', async () => {
    await getResultEntities('a'.repeat(64), KEY, 'result-id', 'Device', () => undefined);
    const url = get.mock.lastCall?.[0] as string;
    expect(url).toBe(`/files/result-files/${'a'.repeat(64)}/triage%23r3~0d9c57aa-0000-0000-0000-000000000000/result-id/Device`);
  });

  it('reads a repo result file from the repo route, keeping the repo url slashes and encoding the result key', async () => {
    await getResultsFile('git.example.test/org/repo', KEY, 'result-id', 'report.txt', () => undefined, true);
    const url = get.mock.lastCall?.[0] as string;
    expect(url).toBe('/repos/result-files/git.example.test/org/repo/triage%23r3~0d9c57aa-0000-0000-0000-000000000000/result-id');
  });

  it("reads a repo's results from the repo results route", async () => {
    get.mockResolvedValue({ status: 200, data: { results: {} } });
    const res = await getRepoResults('git.example.test/org/repo', () => undefined);
    expect(get.mock.lastCall?.[0]).toBe('/repos/results/git.example.test/org/repo');
    expect(res).toEqual({ results: {} });
  });

  it('reports a failed repo results read through the shared error parser and returns null', async () => {
    const error = new Error('boom');
    get.mockRejectedValue(error);
    const handler = vi.fn();
    const res = await getRepoResults('git.example.test/org/repo', handler);
    expect(res).toBeNull();
    expect(parseRequestError).toHaveBeenCalledWith(error, handler, 'Get Repo Results');
  });
});

describe('result file download errors', () => {
  /** An axios error whose response body is the given bytes, as an `arraybuffer` request receives it. */
  const bufferError = (body: string, status = 500) =>
    new axios.AxiosError('failed', 'ERR_BAD_RESPONSE', undefined, undefined, {
      status,
      statusText: '',
      headers: {},
      config: {} as never,
      data: new TextEncoder().encode(body).buffer,
    });

  it("decodes a JSON error body so the API's message reaches the error parser", async () => {
    vi.spyOn(client, 'get').mockRejectedValue(bufferError('{"error":"uiloop injected download failure"}'));
    const handler = vi.fn();
    const res = await getResultsFile('a'.repeat(64), 'tool', 'id', 'report.txt', handler);
    expect(res).toBeNull();
    const [passed, passedHandler, label] = vi.mocked(parseRequestError).mock.lastCall ?? [];
    expect(passedHandler).toBe(handler);
    expect(label).toBe('Get Results File');
    expect((passed as { response: { data: unknown } }).response.data).toEqual({ error: 'uiloop injected download failure' });
  });

  it('decodes a plain text error body to a string', async () => {
    vi.spyOn(client, 'get').mockRejectedValue(bufferError('not json'));
    await getResultsFile('a'.repeat(64), 'tool', 'id', 'report.txt', vi.fn());
    const [passed] = vi.mocked(parseRequestError).mock.lastCall ?? [];
    expect((passed as { response: { data: unknown } }).response.data).toBe('not json');
  });
});
