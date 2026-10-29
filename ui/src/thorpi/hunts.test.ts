import { describe, it, expect, vi, beforeEach, Mock } from 'vitest';

// project imports (the setup file auto-mocks @thorpi/client)
import client, { parseRequestError } from './client';
import { cancelHunt, createHunt, getHuntSystemStatus, listHuntRules } from './hunts';
import { BlankHunt, HuntCreateRequest, HuntEngine } from '@models/entities/hunts';
import { Entities } from '@models/entities/entities';

// the auto-mocked client's post/get are plain vi.fn()s with no `this`, so unbinding is safe
// eslint-disable-next-line @typescript-eslint/unbound-method
const mockedPost = client.post as Mock;
// eslint-disable-next-line @typescript-eslint/unbound-method
const mockedGet = client.get as Mock;
const mockedParse = parseRequestError as Mock;

const request: HuntCreateRequest = {
  name: 'hunt-upx',
  groups: ['corn'],
  engine: HuntEngine.Yara,
  selector: { Rules: ['u1'] },
};

describe('createHunt', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('posts the JSON request to the dedicated hunt route', async () => {
    const hunt = structuredClone(BlankHunt);
    hunt.id = 'h1';
    mockedPost.mockResolvedValue({ status: 200, data: hunt });
    const created = await createHunt(request, () => {});
    // the body is the typed request object, never FormData
    expect(mockedPost).toHaveBeenCalledWith('/entities/hunt/', request);
    expect(created?.id).toBe('h1');
  });

  it('returns null when the response is not a hunt entity', async () => {
    mockedPost.mockResolvedValue({ status: 200, data: { kind: 'Device' } });
    expect(await createHunt(request, () => {})).toBeNull();
  });

  it('funnels failures through parseRequestError and returns null', async () => {
    mockedPost.mockRejectedValue(new Error('quota'));
    const errorHandler = vi.fn();
    expect(await createHunt(request, errorHandler)).toBeNull();
    expect(mockedParse).toHaveBeenCalledWith(expect.any(Error), errorHandler, 'Create Hunt');
  });
});

describe('cancelHunt', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('posts to the cancel route and reports a 204 as success', async () => {
    mockedPost.mockResolvedValue({ status: 204 });
    expect(await cancelHunt('h1', () => {})).toBe(true);
    expect(mockedPost).toHaveBeenCalledWith('/entities/hunt/h1/cancel');
  });

  it('funnels failures through parseRequestError and returns false', async () => {
    mockedPost.mockRejectedValue(new Error('terminal'));
    const errorHandler = vi.fn();
    expect(await cancelHunt('h1', errorHandler)).toBe(false);
    expect(mockedParse).toHaveBeenCalledWith(expect.any(Error), errorHandler, 'Cancel Hunt');
  });
});

describe('listHuntRules', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('hits the id-suffixed route (no trailing slash) and normalizes the skipped fields', async () => {
    // serde skips an empty status and the hydrated fields of a deleted/hidden rule
    const hydrated = { id: 'r1', name: 'upx', kind: Entities.YaraRule, created: '2026-10-17T00:00:00Z', status: ['Noisy'] };
    const idOnly = { id: 'r2' };
    mockedGet.mockResolvedValue({ status: 200, data: { data: [hydrated, idOnly], cursor: null } });
    const { lines, cursor } = await listHuntRules('h1', null, 1000, () => {});
    // unlike the entity list routes, the id-suffixed route takes no trailing slash
    expect(mockedGet).toHaveBeenCalledWith('/entities/hunt/h1/rules', { params: { limit: 1000 } });
    expect(lines).toEqual([
      { ...hydrated, quarantine_exempt: false },
      { id: 'r2', status: [], quarantine_exempt: false },
    ]);
    expect(cursor).toBeNull();
  });

  it('forwards the pagination cursor and hands back the next one', async () => {
    mockedGet.mockResolvedValue({ status: 200, data: { data: [], cursor: 'next' } });
    const { cursor } = await listHuntRules('h1', 'page2', 50, () => {});
    expect(mockedGet).toHaveBeenCalledWith('/entities/hunt/h1/rules', { params: { limit: 50, cursor: 'page2' } });
    expect(cursor).toBe('next');
  });

  it('funnels failures through parseRequestError and returns an empty page', async () => {
    mockedGet.mockRejectedValue(new Error('gone'));
    const errorHandler = vi.fn();
    const { lines, cursor } = await listHuntRules('h1', null, 1000, errorHandler);
    expect(lines).toEqual([]);
    expect(cursor).toBeNull();
    expect(mockedParse).toHaveBeenCalledWith(expect.any(Error), errorHandler, 'List Hunt Rules');
  });
});

describe('getHuntSystemStatus', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('gets the census from the trailing-slash status route with chosen bounds only', async () => {
    const quotas = { max_active_per_user: 2, max_active_global: 8, active_global: 0 };
    mockedGet.mockResolvedValue({
      status: 200,
      data: { ongoing: [], upcoming: [], recent: [], users: [], chunk_backlog: {}, quotas },
    });
    const census = await getHuntSystemStatus(() => {}, 5);
    // only the bounds the caller chose may ride the query string
    expect(mockedGet).toHaveBeenCalledWith('/entities/hunt/status/', { params: { recent: 5 } });
    expect(census?.quotas.max_active_global).toBe(8);
  });

  it('funnels failures through parseRequestError and returns null', async () => {
    mockedGet.mockRejectedValue(new Error('denied'));
    const errorHandler = vi.fn();
    expect(await getHuntSystemStatus(errorHandler)).toBeNull();
    expect(mockedParse).toHaveBeenCalledWith(expect.any(Error), errorHandler, 'Get Hunt System Status');
  });
});
