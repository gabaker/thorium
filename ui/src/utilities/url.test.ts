import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// project imports
import { getApiOrigin, getApiUrl } from './url';

/**
 * Stub `window.location` as a browser reports it for a page URL.
 *
 * @param url - The page URL to describe.
 */
function servePageFrom(url: string): void {
  const parsed = new URL(url);
  vi.stubGlobal('window', {
    location: { protocol: parsed.protocol, hostname: parsed.hostname, port: parsed.port, origin: parsed.origin },
  });
}

describe('getApiUrl', () => {
  beforeEach(() => {
    vi.stubEnv('THORIUM_API_URL', '');
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.unstubAllEnvs();
  });

  it('uses the page origin on the default https port', () => {
    servePageFrom('https://thorium.example.com/files');
    expect(getApiUrl()).toBe('https://thorium.example.com/api');
  });

  it('keeps a custom port', () => {
    servePageFrom('http://thorium.example.com:8080/');
    expect(getApiUrl()).toBe('http://thorium.example.com:8080/api');
  });

  it('keeps the port on localhost without THORIUM_API_URL', () => {
    servePageFrom('http://localhost:5999/auth');
    expect(getApiUrl()).toBe('http://localhost:5999/api');
  });

  it('uses THORIUM_API_URL on localhost', () => {
    servePageFrom('http://localhost:5999/auth');
    vi.stubEnv('THORIUM_API_URL', 'http://localhost:18080/');
    expect(getApiUrl()).toBe('http://localhost:18080/api');
  });

  it('uses THORIUM_API_URL on 127.0.0.1', () => {
    servePageFrom('http://127.0.0.1:5999/auth');
    vi.stubEnv('THORIUM_API_URL', 'http://127.0.0.1:18080');
    expect(getApiUrl()).toBe('http://127.0.0.1:18080/api');
  });

  it('ignores THORIUM_API_URL on other hosts', () => {
    servePageFrom('https://thorium.example.com:8443/');
    vi.stubEnv('THORIUM_API_URL', 'http://localhost:18080');
    expect(getApiUrl()).toBe('https://thorium.example.com:8443/api');
  });
});

describe('getApiOrigin', () => {
  beforeEach(() => {
    vi.stubEnv('THORIUM_API_URL', '');
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.unstubAllEnvs();
  });

  it('uses the page origin on the default https port', () => {
    servePageFrom('https://thorium.example.com/files');
    expect(getApiOrigin()).toBe('https://thorium.example.com');
  });

  it('keeps a custom port', () => {
    servePageFrom('http://thorium.example.com:8080/');
    expect(getApiOrigin()).toBe('http://thorium.example.com:8080');
  });

  it('keeps the port on localhost without THORIUM_API_URL', () => {
    servePageFrom('http://localhost:5999/auth');
    expect(getApiOrigin()).toBe('http://localhost:5999');
  });

  it('uses THORIUM_API_URL on localhost and strips trailing slashes', () => {
    servePageFrom('http://localhost:5999/auth');
    vi.stubEnv('THORIUM_API_URL', 'http://localhost:18080///');
    expect(getApiOrigin()).toBe('http://localhost:18080');
  });

  it('uses THORIUM_API_URL on 127.0.0.1', () => {
    servePageFrom('http://127.0.0.1:5999/auth');
    vi.stubEnv('THORIUM_API_URL', 'http://127.0.0.1:18080');
    expect(getApiOrigin()).toBe('http://127.0.0.1:18080');
  });

  it('ignores THORIUM_API_URL on other hosts', () => {
    servePageFrom('https://thorium.example.com:8443/');
    vi.stubEnv('THORIUM_API_URL', 'http://localhost:18080');
    expect(getApiOrigin()).toBe('https://thorium.example.com:8443');
  });

  it('is empty outside a browser, so the API URL is the relative /api path', () => {
    vi.stubGlobal('window', undefined);
    expect(getApiOrigin()).toBe('');
    expect(getApiUrl()).toBe('/api');
  });
});
