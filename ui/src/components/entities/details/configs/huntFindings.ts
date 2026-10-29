// project imports
// type-only on purpose: a value import would pull the Tabs component (React,
// styled-components) into this helper and break the node-env vitest suite
import type { TabItem } from '@components/shared/tabs';
import { Entities } from '@models/entities';
import { Hunt, HUNT_ENGINES, HUNT_ID_TAG } from '@models/entities/hunts';
import { Filters } from '@models/search';
import { SUBMITTER_TAG } from '@models/tags';

// Pure helpers for the hunt details page's findings card. React-free on purpose so the
// node-env vitest suite can pin the engine gating, the tab strip, and the inline hit
// list's filters.

/**
 * The tag keys hidden on hit rows inside the inline findings panel only.
 *
 * Every row in the panel carries the same `HuntId` (the panel is already pinned to one
 * hunt) and the submitter is a row column, so both chips are pure noise there; the full
 * hit browse page keeps them because its rows can mix hunts.
 */
export const INLINE_HIT_HIDDEN_TAG_KEYS: string[] = [HUNT_ID_TAG, SUBMITTER_TAG];

/** The findings card's two views: the inline hit list and the frozen rule snapshot. */
export type HuntFindingsView = 'hits' | 'rules';

/**
 * Build the findings card's tab strip: Hits (the default view) and Frozen Rules.
 *
 * The Frozen Rules badge carries the exact snapshot size (`metadata.Hunt.rules` is
 * resolved and frozen at creation), so it never drifts the way a drained-so-far total
 * would; the Hits view has no stable count (counters are telemetry), so no badge.
 *
 * @param frozenCount - The number of rules frozen into the hunt at creation.
 * @returns The tab descriptors for the shared Tabs component.
 */
export const huntFindingsTabs = (frozenCount: number): TabItem<HuntFindingsView>[] => [
  { key: 'hits', label: 'Hits', tip: 'The findings of this hunt' },
  { key: 'rules', label: 'Frozen Rules', count: frozenCount, tip: 'The rules frozen into this hunt at creation' },
];

/**
 * Resolve the entity kind of a hunt's findings, or undefined when there is none to list.
 *
 * The blank placeholder (the details factory leaves it in place until the hunt lands)
 * has no findings record, and an engine this UI build does not know (a newer server
 * during a rolling upgrade) has no hit kind to list — both hide the hits view instead
 * of rendering a broken panel.
 *
 * @param hunt - The hunt whose finding kind to resolve.
 * @returns The engine's hit kind, or undefined for the placeholder or an unknown engine.
 */
export function huntHitKind(hunt: Hunt): Entities | undefined {
  // the blank placeholder's empty id must never pin an empty HuntId filter
  if (hunt.id === '') {
    return undefined;
  }
  // the engine registry picks the finding kind, keeping the panel engine-generic
  return HUNT_ENGINES[hunt.metadata.Hunt.engine]?.hitKind;
}

/**
 * Build the inline hit list's filters: the `HuntId` tag pin plus the page size.
 *
 * The hit route stamps `HuntId` on every decorated finding, so this tag filter is the
 * exact findings record (counters are telemetry and can drift under replay).
 *
 * @param huntId - The hunt whose findings to list.
 * @param limit - The page size for the list.
 * @returns The filters for the entity listing.
 */
export function huntHitsFilters(huntId: string, limit: number): Filters {
  return { tags: { [HUNT_ID_TAG]: [huntId] }, limit };
}
