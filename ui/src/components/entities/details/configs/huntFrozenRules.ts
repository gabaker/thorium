// project imports
import { Clause, ClauseIsMulti } from '@components/shared/inputs/omnibar/ClauseTypes';
import { TimeSelection } from '@components/shared/inputs/omnibar/timepicker/utils';
import { createdInRange } from './ruleSetRules';
import { HuntRuleLine } from '@models/entities/hunts';
import { getDetailsBasePathByEntity } from '@components/entities/details/EntityDetailsRoutes';

// Pure helpers for the hunt details page's frozen rules section. React-free on purpose:
// the frozen snapshot is drained once (it is immutable) and then filtered/paged entirely
// client side, so all of its behavior lives here where the node-env vitest suite can pin
// it. The paging math and time predicate are identical to the ruleset members section,
// so they are re-exported rather than duplicated.
export { clampPage, createdInRange, pageCount } from './ruleSetRules';

/**
 * Build the details href for a frozen rule line, or undefined when it must not link.
 *
 * A line can lack a kind entirely (the rule was deleted after the freeze or is invisible
 * to the caller) or carry a kind this build cannot route (a kind served by a newer API
 * than this UI shipped with). Those lines render linkless rather than linking to a page
 * that can never load.
 *
 * @param line - The frozen rule line to link.
 * @returns The internal details url, or undefined when no link should render.
 */
export const frozenRuleDetailsUrl = (line: HuntRuleLine): string | undefined => {
  // a deleted or hidden rule hydrates no kind, so it can never route
  if (!line.kind) {
    return undefined;
  }
  // resolve the kind's details base path; undefined means this kind cannot be routed
  const basePath = getDetailsBasePathByEntity(line.kind);
  return basePath ? `${basePath}/${line.id}` : undefined;
};

/**
 * Sort a copy of a frozen rule list by name (never the React state array in place).
 *
 * @param lines - The frozen rule lines to sort.
 * @returns A new array sorted by rule name (id fallback for id-only deleted/hidden lines).
 */
export const sortFrozenRules = (lines: HuntRuleLine[]): HuntRuleLine[] =>
  [...lines].sort((a: HuntRuleLine, b: HuntRuleLine) => (a.name ?? a.id).localeCompare(b.name ?? b.id));

/**
 * Decide whether a frozen rule line passes every filtering clause (clauses AND together).
 *
 * Free `text` clauses substring-match the rule name OR its id case-insensitively (so
 * id-only deleted/hidden lines stay findable), `kind` clauses match the line's engine,
 * and anything else (the paging `limit` clause, fields a future bar might add) never
 * excludes a line.
 *
 * @param line - The frozen rule line to test.
 * @param clauses - The omnibar clauses to test against.
 * @returns True when the line passes every filtering clause.
 */
export function frozenRuleMatchesClauses(line: HuntRuleLine, clauses: Clause[]): boolean {
  return clauses.every((clause) => {
    // free text matches the name or the id so deleted/hidden lines stay findable by id
    if (clause.field === 'text' && !ClauseIsMulti(clause)) {
      const text = clause.value.value.toLowerCase();
      return (line.name ?? '').toLowerCase().includes(text) || line.id.toLowerCase().includes(text);
    }
    // kind filters by the line's engine (`is` carries one value, `is one of` carries many);
    // a line with no hydrated kind matches no engine filter on purpose
    if (clause.field === 'kind') {
      const kinds = ClauseIsMulti(clause) ? clause.value.values : [clause.value.value];
      return line.kind !== undefined && kinds.includes(line.kind);
    }
    // the paging limit clause and unknown fields never exclude a line
    return true;
  });
}

/**
 * Filter and name-sort a hunt's frozen rule lines for display.
 *
 * A deleted-after-freeze (or hidden) line hydrates no `created`, so a bounded time
 * range hides it while the default All Time keeps it visible.
 *
 * @param lines - The drained frozen rule lines.
 * @param clauses - The omnibar clauses to filter with.
 * @param time - The time picker selection narrowing by rule creation date.
 * @returns The lines passing every clause and the time range, sorted by name (id fallback).
 */
export function filterFrozenRules(lines: HuntRuleLine[], clauses: Clause[], time: TimeSelection): HuntRuleLine[] {
  return sortFrozenRules(lines.filter((line) => frozenRuleMatchesClauses(line, clauses) && createdInRange(line.created, time)));
}
