import { describe, expect, it, vi } from 'vitest';

// project imports
import { bannedIds, loadBanNotifications } from './notifications';
import { EntityNotification } from '@models/agentic';

const note = (id: string) => ({ id, msg: id, level: 'Error' }) as unknown as EntityNotification;

describe('ban notifications', () => {
  it('picks only entities with bans', () => {
    const ban = { a: {} as never };
    expect(
      bannedIds([
        { id: 'b', bans: ban },
        { id: 'a', bans: {} },
        { id: 'c', bans: undefined },
        { id: 'b', bans: ban },
      ]),
    ).toEqual(['b']);
  });
  it('loads each id once and keys results by id', async () => {
    const load = vi.fn((id: string) => Promise.resolve([note(`n-${id}`)]));
    const result = await loadBanNotifications(['x', 'y'], load);
    expect(load).toHaveBeenCalledTimes(2);
    expect(result.byId['x']?.[0]?.id).toBe('n-x');
    expect(result.byId['y']?.[0]?.id).toBe('n-y');
    expect(result.errors).toEqual([]);
  });
  it('reports load errors once instead of dropping them', async () => {
    const load = (_id: string, onError: (message: string) => void) => {
      onError('List Entity Notifications: failed');
      return Promise.resolve([]);
    };
    const result = await loadBanNotifications(['x', 'y'], load);
    expect(result.errors).toEqual(['List Entity Notifications: failed']);
    expect(result.byId['x']).toEqual([]);
  });
  it('loads nothing for no ids', async () => {
    const load = vi.fn(() => Promise.resolve([]));
    expect(await loadBanNotifications([], load)).toEqual({ byId: {}, errors: [] });
    expect(load).not.toHaveBeenCalled();
  });
});
