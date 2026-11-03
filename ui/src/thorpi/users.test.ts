import { AxiosError, AxiosResponse } from 'axios';
import { afterEach, describe, it, expect, vi } from 'vitest';

// project imports
import client from './client';
import { checkSession, parseRetryAfter, whoami } from './users';
import { SessionState } from '@models/users';

describe('parseRetryAfter', () => {
  it('reads a numeric Retry-After header from a string', () => {
    expect(parseRetryAfter({ 'retry-after': '600' })).toBe(600);
  });

  it('reads a numeric Retry-After header from a number', () => {
    expect(parseRetryAfter({ 'retry-after': 42 })).toBe(42);
  });

  it('defaults to 0 when missing, non-positive, or unparseable', () => {
    expect(parseRetryAfter({})).toBe(0);
    expect(parseRetryAfter({ 'retry-after': 'soon' })).toBe(0);
    expect(parseRetryAfter({ 'retry-after': '0' })).toBe(0);
    expect(parseRetryAfter({ 'retry-after': '-5' })).toBe(0);
    expect(parseRetryAfter(undefined)).toBe(0);
    expect(parseRetryAfter(null)).toBe(0);
  });
});

/**
 * Build the error axios raises for a response with an error status.
 *
 * @param status - The HTTP status the server answered with.
 * @returns The error.
 */
function statusError(status: number): AxiosError {
  return new AxiosError(`status ${status}`, 'ERR_BAD_RESPONSE', undefined, undefined, { status, data: {} } as AxiosResponse);
}

describe('checkSession', () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });
  it('returns the user when the server accepts the session', async () => {
    vi.spyOn(client, 'get').mockResolvedValue({ status: 200, data: { username: 'alice' } });
    await expect(checkSession()).resolves.toEqual({ state: SessionState.Valid, user: { username: 'alice' } });
  });
  it.each([401, 403])('finds the session expired when the server refuses it with %i', async (status) => {
    vi.spyOn(console, 'log').mockImplementation(() => undefined);
    vi.spyOn(client, 'get').mockRejectedValue(statusError(status));
    await expect(checkSession()).resolves.toEqual({ state: SessionState.Expired });
  });
  it('finds the server unavailable, not the session expired, on a network error', async () => {
    vi.spyOn(console, 'log').mockImplementation(() => undefined);
    vi.spyOn(client, 'get').mockRejectedValue(new AxiosError('Network Error', 'ERR_NETWORK'));
    await expect(checkSession()).resolves.toEqual({ state: SessionState.Unavailable });
  });
  it.each([500, 502, 503])('finds the server unavailable, not the session expired, on a %i', async (status) => {
    vi.spyOn(console, 'log').mockImplementation(() => undefined);
    vi.spyOn(client, 'get').mockRejectedValue(statusError(status));
    await expect(checkSession()).resolves.toEqual({ state: SessionState.Unavailable });
  });
  it('lets whoami answer with the user, or null when the session cannot be confirmed', async () => {
    vi.spyOn(console, 'log').mockImplementation(() => undefined);
    vi.spyOn(client, 'get').mockResolvedValueOnce({ status: 200, data: { username: 'alice' } });
    await expect(whoami()).resolves.toEqual({ username: 'alice' });
    vi.spyOn(client, 'get').mockRejectedValueOnce(statusError(401));
    await expect(whoami()).resolves.toBeNull();
  });
});
