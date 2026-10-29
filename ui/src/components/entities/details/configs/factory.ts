import { JSX } from 'react';

// project imports
import { MetadataComponent, SectionComponent } from '../EntityDetails';
import { getEntity } from '@thorpi/entities';
import { EntityTypeMap } from '@models/entities';

// This module holds the shared details-config type and factory. It deliberately imports no
// per-kind config files: the config files import `makeGetEntityDetails` from here, and
// `configs.ts` imports the config files. Keeping the factory out of `configs.ts` breaks the
// import cycle that would otherwise leave `makeGetEntityDetails` in its temporal dead zone
// when a config invokes it at module-evaluation time.

export type EntityDetailsConfig<T extends keyof EntityTypeMap> = {
  getEntityDetails: (entityID: string, setError: (err: string) => void, updateEntity: (entity: EntityTypeMap[T]) => void) => void;
  EntityMetaInfo: MetadataComponent<T>;
  BlankEntity: EntityTypeMap[T];
  icon: (size: number) => JSX.Element;
  supportsGraphic?: boolean;
  /**
   * Optional replacement for the Associations graph card at the bottom of the details page.
   *
   * When set, the component owns the whole section (its own card and title) and the
   * association graph (and its tree data fetching) is never mounted for this kind.
   */
  AssociationsSection?: SectionComponent<T>;
  /**
   * Auto-refetch the entity while this returns a millisecond interval; null stops polling.
   *
   * Re-derived from each freshly fetched entity (so a kind can poll only while it is in an
   * active state, e.g. a Running hunt), paused during editing so a landing refetch never
   * clobbers pending edits, and cleared on unmount.
   */
  refetchIntervalMs?: (entity: EntityTypeMap[T]) => number | null;
};

/**
 * Build a details-config `getEntityDetails` function for a single entity kind.
 *
 * Every per-kind details config fetches the same way: load the entity by id and only
 * hand it back once its discriminant `kind` matches the expected one. This factory
 * captures that shared shape so each config only needs to name its kind.
 *
 * @template T - The entity kind key into `EntityTypeMap`.
 * @param kind - The entity kind the fetched entity must match to be accepted.
 * @returns A `getEntityDetails` function matching `EntityDetailsConfig<T>`.
 */
export const makeGetEntityDetails =
  <T extends keyof EntityTypeMap>(kind: T): EntityDetailsConfig<T>['getEntityDetails'] =>
  (entityID, setError, updateEntity) => {
    void getEntity(entityID, setError).then((data) => {
      if (data && data.kind == kind) {
        updateEntity(data as EntityTypeMap[T]);
      }
    });
  };
