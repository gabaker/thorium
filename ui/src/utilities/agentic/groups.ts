// project imports
import { AnyAgenticEntity } from '@models/agentic';

/** The parts of an entity the group helpers read. */
type Grouped = Pick<AnyAgenticEntity, 'id' | 'name' | 'groups'>;

/**
 * List the chosen groups an entity is not in.
 *
 * An edge target must be in every group of the entity that links to it, so any group listed here
 * makes the entity an invalid target.
 *
 * @param entity - The entity that would be linked to.
 * @param groups - The groups the linking entity is in.
 * @returns The groups the entity lacks, in the order given.
 */
export function missingGroups(entity: Pick<AnyAgenticEntity, 'groups'>, groups: string[]): string[] {
  return groups.filter((group) => !entity.groups.includes(group));
}

/**
 * Keep only the selected ids whose entity is in every chosen group.
 *
 * @param entities - The entities that can be selected.
 * @param ids - The selected entity ids.
 * @param groups - The groups the linking entity is in.
 * @returns The selected ids that are still valid; ids with no matching entity are kept.
 */
export function keepCovering(entities: Grouped[], ids: string[], groups: string[]): string[] {
  const byId = new Map(entities.map((entity) => [entity.id, entity]));
  return ids.filter((id) => {
    const entity = byId.get(id);
    return !entity || missingGroups(entity, groups).length === 0;
  });
}

/**
 * Find the names that more than one of the entities has.
 *
 * @param entities - The entities to compare.
 * @returns The names that repeat.
 */
export function duplicateNames(entities: Grouped[]): Set<string> {
  const seen = new Set<string>();
  const repeated = new Set<string>();
  for (const entity of entities) {
    if (seen.has(entity.name)) repeated.add(entity.name);
    seen.add(entity.name);
  }
  return repeated;
}

/**
 * Describe an entity in a picker so same-named entities can be told apart and an entity that
 * can't be linked says why.
 *
 * @param entity - The entity to describe.
 * @param groups - The groups the linking entity is in.
 * @param duplicate - Whether another entity in the same picker has this name.
 * @returns The note, and whether the entity is unavailable because it lacks a chosen group.
 */
export function describeOption(entity: Grouped, groups: string[], duplicate: boolean): { note: string; disabled: boolean } {
  const missing = missingGroups(entity, groups);
  // a short id is the only thing that separates same-named entities in the same groups
  const shortId = duplicate ? `; #${entity.id.slice(0, 8)}` : '';
  if (missing.length > 0) return { note: `not in ${missing.join(', ')}${shortId}`, disabled: true };
  return { note: `in ${entity.groups.join(', ')}${shortId}`, disabled: false };
}
