import { useEffect, useMemo, useState } from 'react';

// project imports
import { banCount } from './format';
import { listEntityNotifications } from '@thorpi/agentic';
import { EntityNotification } from '@models/agentic';
import { ModelBan } from '@models/modelconfigs';

/** An entity id with the bans currently on that entity. */
export interface BannedCandidate {
  /** The entity's id */
  id: string;
  /** The bans on the entity by id */
  bans: Record<string, ModelBan> | undefined;
}

/** The ban notifications loaded for a set of entities. */
export interface BanNotifications {
  /** The notifications of each entity, by entity id */
  byId: Record<string, EntityNotification[]>;
  /** The load errors, with duplicates removed; empty when every load succeeded */
  errors: string[];
}

/** The loader of one entity's notifications; matches {@link listEntityNotifications}. */
export type NotificationLoader = (id: string, errorHandler: (error: string) => void) => Promise<EntityNotification[]>;

/**
 * Pick the ids of the entities that have bans.
 *
 * Ban notifications are kept in step with the bans, so an entity with no bans has none to load.
 *
 * @param candidates - The entities and their bans.
 * @returns The ids of the entities with at least one ban, sorted and without duplicates.
 */
export function bannedIds(candidates: BannedCandidate[]): string[] {
  return [...new Set(candidates.filter((candidate) => banCount(candidate.bans) > 0).map((candidate) => candidate.id))].sort();
}

/**
 * Load the ban notifications of each entity and collect any load errors.
 *
 * @param ids - The entity ids to load notifications for.
 * @param load - Loads one entity's notifications.
 * @returns The notifications by entity id and the distinct load errors.
 */
export async function loadBanNotifications(ids: string[], load: NotificationLoader = listEntityNotifications): Promise<BanNotifications> {
  // collect every error message raised while loading
  const errors = new Set<string>();
  const loaded = await Promise.all(ids.map((id) => load(id, (message) => errors.add(message))));
  // pair each entity with its notifications
  const byId: Record<string, EntityNotification[]> = {};
  ids.forEach((id, index) => {
    byId[id] = loaded[index] ?? [];
  });
  return { byId, errors: [...errors] };
}

/**
 * Load the ban notifications of the banned entities once for a page.
 *
 * Reloads only when the set of banned entities changes, so switching tabs does not repeat requests.
 *
 * @param candidates - Every entity shown on the page with its bans.
 * @returns The notifications by entity id and a message for any load error, or an empty string.
 */
export function useBanNotifications(candidates: BannedCandidate[]): { byId: Record<string, EntityNotification[]>; error: string } {
  const [state, setState] = useState<BanNotifications>({ byId: {}, errors: [] });
  const key = bannedIds(candidates).join(',');
  const ids = useMemo(() => (key === '' ? [] : key.split(',')), [key]);
  useEffect(() => {
    let cancelled = false;
    void loadBanNotifications(ids).then((loaded) => {
      // a load that was superseded or unmounted must not overwrite the current one
      if (!cancelled) setState(loaded);
    });
    return () => {
      cancelled = true;
    };
  }, [ids]);
  return { byId: state.byId, error: state.errors.join('; ') };
}
