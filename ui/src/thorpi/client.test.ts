import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// The global test setup mocks the client module; these tests exercise the real one.
vi.unmock('@thorpi/client');

/** The parts of `window.location` the client reads. */
interface FakeLocation {
  protocol: string;
  hostname: string;
  port: string;
  origin: string;
}

/**
 * Build a fake `window.location` for a page URL, the way a browser reports it.
 *
 * @param url - The page URL to describe.
 * @returns The location fields the client reads.
 */
function locationOf(url: string): FakeLocation {
  const parsed = new URL(url);
  return { protocol: parsed.protocol, hostname: parsed.hostname, port: parsed.port, origin: parsed.origin };
}

/**
 * Import a fresh copy of the real client module as if the page were served from `url`.
 *
 * @param url - The page URL, or `undefined` for a non-browser environment.
 * @param apiUrl - The `THORIUM_API_URL` dev-server override, if any.
 * @returns The `baseURL` the module's axios instance was created with.
 */
async function baseURLFor(url: string | undefined, apiUrl?: string): Promise<string | undefined> {
  vi.resetModules();
  if (url !== undefined) vi.stubGlobal('window', { location: locationOf(url) });
  if (apiUrl !== undefined) vi.stubEnv('THORIUM_API_URL', apiUrl);
  const mod = await import('./client');
  return mod.default.defaults.baseURL;
}

describe('API base URL', () => {
  beforeEach(() => {
    vi.stubEnv('THORIUM_API_URL', '');
    // keep the dev-server warning out of the test output
    vi.spyOn(console, 'warn').mockImplementation(() => {});
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.unstubAllEnvs();
    vi.restoreAllMocks();
  });

  it('uses the page origin on the default https port', async () => {
    expect(await baseURLFor('https://thorium.example.com/files')).toBe('https://thorium.example.com/api');
  });

  it('uses the page origin on the default http port', async () => {
    expect(await baseURLFor('http://thorium.example.com/')).toBe('http://thorium.example.com/api');
  });

  it('keeps a custom https port', async () => {
    expect(await baseURLFor('https://thorium.example.com:8443/files')).toBe('https://thorium.example.com:8443/api');
  });

  it('keeps the port of an http page on 8080', async () => {
    expect(await baseURLFor('http://thorium.example.com:8080/')).toBe('http://thorium.example.com:8080/api');
  });

  it('keeps the port on localhost without THORIUM_API_URL', async () => {
    expect(await baseURLFor('http://localhost:3000/')).toBe('http://localhost:3000/api');
  });

  it('uses THORIUM_API_URL on localhost', async () => {
    expect(await baseURLFor('http://localhost:3000/', 'http://localhost:8000')).toBe('http://localhost:8000/api');
  });

  it('uses THORIUM_API_URL on 127.0.0.1', async () => {
    expect(await baseURLFor('http://127.0.0.1:3000/', 'http://127.0.0.1:8000')).toBe('http://127.0.0.1:8000/api');
  });

  it('strips trailing slashes from THORIUM_API_URL', async () => {
    expect(await baseURLFor('http://localhost:3000/', 'http://localhost:8000///')).toBe('http://localhost:8000/api');
  });

  it('ignores THORIUM_API_URL on other hosts', async () => {
    expect(await baseURLFor('https://thorium.example.com:8443/', 'http://localhost:8000')).toBe('https://thorium.example.com:8443/api');
  });

  it('targets the relative /api path outside a browser', async () => {
    expect(await baseURLFor(undefined)).toBe('/api');
  });
});

describe('dev server warning', () => {
  beforeEach(() => {
    vi.stubEnv('THORIUM_API_URL', '');
    vi.stubEnv('DEV', true);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.unstubAllEnvs();
    vi.restoreAllMocks();
  });

  it('warns once, naming THORIUM_API_URL, when the dev server falls back to the page origin', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    expect(await baseURLFor('http://localhost:8000/')).toBe('http://localhost:8000/api');
    expect(warn).toHaveBeenCalledTimes(1);
    expect(warn.mock.calls[0][0]).toContain('export THORIUM_API_URL=');
  });

  it('warns on a non-loopback dev host even with THORIUM_API_URL set', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    await baseURLFor('http://10.0.0.5:8000/', 'http://localhost:8080');
    expect(warn).toHaveBeenCalledTimes(1);
  });

  it('stays quiet when THORIUM_API_URL applies', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    await baseURLFor('http://localhost:8000/', 'http://localhost:8080');
    expect(warn).not.toHaveBeenCalled();
  });

  it('stays quiet in production builds', async () => {
    vi.stubEnv('DEV', false);
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    await baseURLFor('http://localhost:8000/');
    expect(warn).not.toHaveBeenCalled();
  });
});
