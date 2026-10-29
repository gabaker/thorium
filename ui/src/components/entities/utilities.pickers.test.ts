import { describe, it, expect, vi, beforeEach, Mock } from 'vitest';

// mock the entities client so the picker helpers never touch the API
vi.mock('@thorpi/entities', () => ({
  listEntities: vi.fn(),
}));

// project imports
import { EntityListLine, listEntities } from '@thorpi/entities';
import { getAvailableEntities, getAvailableVendors } from './utilities';
import { Entities } from '@models/entities/entities';

const mockedList = listEntities as Mock;

// thin summary lines exactly as the details=false route serializes them (no groups/tags/metadata)
const lines: EntityListLine[] = [
  { id: '11111111-1111-1111-1111-111111111111', name: 'upx', kind: Entities.YaraRule, created: '2026-10-17T00:00:00Z' },
  { id: '22222222-2222-2222-2222-222222222222', name: 'proc_dump', kind: Entities.SigmaRule, created: '2026-10-17T00:00:00Z' },
];

describe('getAvailableEntities', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('lists one bounded page of thin lines (details=false) and builds the id -> name map', async () => {
    mockedList.mockResolvedValue({ entityList: lines, entityCursor: null });
    const updateMap = vi.fn();
    // the declared return type is the thin line shape, not full EntityTypes
    const returned: EntityListLine[] = await getAvailableEntities([Entities.YaraRule, Entities.SigmaRule], updateMap);
    // details must stay false — the picker map never needs rule sources or hydrated members
    expect(mockedList).toHaveBeenCalledWith(
      { kinds: [Entities.YaraRule, Entities.SigmaRule], limit: 10000 },
      console.log,
      false,
      null,
    );
    expect(updateMap).toHaveBeenCalledWith({
      '11111111-1111-1111-1111-111111111111': 'upx',
      '22222222-2222-2222-2222-222222222222': 'proc_dump',
    });
    expect(returned).toEqual(lines);
  });

  it('filters lines whose kind was not requested out of the map', async () => {
    mockedList.mockResolvedValue({ entityList: lines, entityCursor: null });
    const updateMap = vi.fn();
    await getAvailableEntities([Entities.YaraRule], updateMap);
    expect(updateMap).toHaveBeenCalledWith({ '11111111-1111-1111-1111-111111111111': 'upx' });
  });

  it('hands an empty map to the setter when the list comes back empty', async () => {
    mockedList.mockResolvedValue({ entityList: [], entityCursor: null });
    const updateMap = vi.fn();
    await getAvailableEntities([Entities.Vendor], updateMap);
    expect(updateMap).toHaveBeenCalledWith({});
  });
});

describe('getAvailableVendors', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('delegates to getAvailableEntities for the Vendor kind', async () => {
    mockedList.mockResolvedValue({ entityList: [], entityCursor: null });
    const updateMap = vi.fn();
    await getAvailableVendors(updateMap);
    expect(mockedList).toHaveBeenCalledWith({ kinds: [Entities.Vendor], limit: 10000 }, console.log, false, null);
  });
});
