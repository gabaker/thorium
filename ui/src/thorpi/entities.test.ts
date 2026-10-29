import { describe, it, expect, vi, beforeEach, Mock } from 'vitest';

// project imports (the setup file auto-mocks @thorpi/client)
import client, { parseRequestError } from './client';
import { clearQuarantineExemption, deleteEntityTags, EntityListLine, listEntities, setQuarantineExemption } from './entities';
import { Entities } from '@models/entities/entities';
import { Filters } from '@models/search';

// the auto-mocked client's get/post/delete are plain vi.fn()s with no `this`, so unbinding is safe
// eslint-disable-next-line @typescript-eslint/unbound-method
const mockedGet = client.get as Mock;
// eslint-disable-next-line @typescript-eslint/unbound-method
const mockedPost = client.post as Mock;
// eslint-disable-next-line @typescript-eslint/unbound-method
const mockedDelete = client.delete as Mock;
const mockedParse = parseRequestError as Mock;

// a thin summary line exactly as the details=false route serializes it (no groups/tags/metadata)
const line: EntityListLine = {
  id: '11111111-1111-1111-1111-111111111111',
  name: 'upx',
  kind: Entities.YaraRule,
  created: '2026-10-17T00:00:00Z',
};

// These tests pin the list route paths — both require the trailing slash (the bare
// /entities path is a 404) — and the thin shape of the summary lines.
describe('listEntities', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('hits the trailing-slash summary route and returns the thin list lines', async () => {
    mockedGet.mockResolvedValue({ status: 200, data: { data: [line], cursor: null } });
    const filters: Filters = { kinds: [Entities.YaraRule], limit: 10 };
    const { entityList, entityCursor } = await listEntities(filters, () => {}, false, null);
    // the bare /entities path is a 404 — the trailing slash is the route contract
    expect(mockedGet).toHaveBeenCalledWith('/entities/', { params: filters });
    expect(entityList).toEqual([line]);
    expect(entityCursor).toBeNull();
  });

  it('hits the trailing-slash details route when full entities are requested', async () => {
    mockedGet.mockResolvedValue({ status: 200, data: { data: [], cursor: null } });
    await listEntities({}, () => {}, true, null);
    expect(mockedGet).toHaveBeenCalledWith('/entities/details/', { params: {} });
  });

  it('forwards the pagination cursor and hands back the next one', async () => {
    mockedGet.mockResolvedValue({ status: 200, data: { data: [line], cursor: 'next' } });
    const { entityCursor } = await listEntities({}, () => {}, false, 'page2');
    expect(mockedGet).toHaveBeenCalledWith('/entities/', { params: { cursor: 'page2' } });
    expect(entityCursor).toBe('next');
  });

  it('funnels failures through parseRequestError and returns an empty page', async () => {
    mockedGet.mockRejectedValue(new Error('boom'));
    const errorHandler = vi.fn();
    const { entityList, entityCursor } = await listEntities({}, errorHandler, false, null);
    expect(entityList).toEqual([]);
    expect(entityCursor).toBeNull();
    expect(mockedParse).toHaveBeenCalledWith(expect.any(Error), errorHandler, 'List Entity');
  });
});

describe('deleteEntityTags', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('sends the tag delete body in the DELETE data option and reports a 204 as success', async () => {
    mockedDelete.mockResolvedValue({ status: 204 });
    const ok = await deleteEntityTags('e1', { YaraRuleStatus: ['Noisy', 'Slow'] }, ['corn'], () => {});
    // the route takes a JSON body on the DELETE, so it must ride in axios' `data`
    expect(mockedDelete).toHaveBeenCalledWith('/entities/tags/e1', {
      data: { groups: ['corn'], tags: { YaraRuleStatus: ['Noisy', 'Slow'] } },
    });
    expect(ok).toBe(true);
  });

  it('funnels failures through parseRequestError and returns false', async () => {
    mockedDelete.mockRejectedValue(new Error('denied'));
    const errorHandler = vi.fn();
    expect(await deleteEntityTags('e1', {}, [], errorHandler)).toBe(false);
    expect(mockedParse).toHaveBeenCalledWith(expect.any(Error), errorHandler, 'Delete Entity Tags');
  });
});

describe('setQuarantineExemption', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('posts to the exemption route and reports a 204 as success', async () => {
    mockedPost.mockResolvedValue({ status: 204 });
    expect(await setQuarantineExemption('r1', () => {})).toBe(true);
    expect(mockedPost).toHaveBeenCalledWith('/entities/rules/r1/quarantine-exemption');
  });

  it('funnels failures through parseRequestError and returns false', async () => {
    mockedPost.mockRejectedValue(new Error('not an admin'));
    const errorHandler = vi.fn();
    expect(await setQuarantineExemption('r1', errorHandler)).toBe(false);
    expect(mockedParse).toHaveBeenCalledWith(expect.any(Error), errorHandler, 'Set Quarantine Exemption');
  });
});

describe('clearQuarantineExemption', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('deletes the exemption and reports a 204 as success', async () => {
    mockedDelete.mockResolvedValue({ status: 204 });
    expect(await clearQuarantineExemption('r1', () => {})).toBe(true);
    expect(mockedDelete).toHaveBeenCalledWith('/entities/rules/r1/quarantine-exemption');
  });

  it('funnels failures through parseRequestError and returns false', async () => {
    mockedDelete.mockRejectedValue(new Error('not an admin'));
    const errorHandler = vi.fn();
    expect(await clearQuarantineExemption('r1', errorHandler)).toBe(false);
    expect(mockedParse).toHaveBeenCalledWith(expect.any(Error), errorHandler, 'Clear Quarantine Exemption');
  });
});
