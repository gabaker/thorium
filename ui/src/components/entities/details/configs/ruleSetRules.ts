// project imports
import { Clause, ClauseIsMulti } from '@components/shared/inputs/omnibar/ClauseTypes';
import { TimeSelection, TimeSelectionToDateRange } from '@components/shared/inputs/omnibar/timepicker/utils';
import { RuleSetMember } from '@models/entities/rulesets';
import { getDetailsBasePathByEntity } from '@components/entities/details/EntityDetailsRoutes';

// Pure helpers for the ruleset details page's member rules section. React-free on purpose:
// the member list is filtered/paged entirely client side (rulesets arrive fully hydrated),
// so all of its behavior lives here where the node-env vitest suite can pin it.

/**
 * Build the details href for a rule set member, or undefined when it must not link.
 *
 * A member can carry a kind this build cannot route: a just-saved pending fallback whose
 * kind was never resolved (see `resolvePendingMembers` in the rule set details config), or
 * a kind served by a newer API than this UI shipped with. Those members render linkless
 * rather than linking to the wrong engine's details page.
 *
 * @param member - The member to link.
 * @returns The internal details url, or undefined when no link should render.
 */
export const memberDetailsUrl = (member: RuleSetMember): string | undefined => {
  // resolve the kind's details base path; undefined means this kind cannot be routed
  const basePath = getDetailsBasePathByEntity(member.kind);
  return basePath ? `${basePath}/${member.id}` : undefined;
};

/**
 * Sort a copy of a member rules list by name (never the React state array in place).
 *
 * @param rules - The member rules to sort.
 * @returns A new array sorted by member name (id fallback when a name is missing).
 */
export const sortMembersList = (rules: RuleSetMember[]): RuleSetMember[] =>
  [...rules].sort((a: RuleSetMember, b: RuleSetMember) => (a.name ?? a.id).localeCompare(b.name ?? b.id));

/**
 * Decide whether a member rule passes every filtering clause (clauses AND together).
 *
 * Free `text` clauses substring-match the member name case-insensitively, `kind` clauses
 * match the member's engine, and anything else (the paging `limit` clause, fields a future
 * bar might add) never excludes a member.
 *
 * @param rule - The member rule to test.
 * @param clauses - The omnibar clauses to test against.
 * @returns True when the member passes every filtering clause.
 */
export function ruleMatchesClauses(rule: RuleSetMember, clauses: Clause[]): boolean {
  return clauses.every((clause) => {
    // free text filters by case-insensitive name substring (id fallback mirrors sortMembersList)
    if (clause.field === 'text' && !ClauseIsMulti(clause)) {
      return (rule.name ?? rule.id).toLowerCase().includes(clause.value.value.toLowerCase());
    }
    // kind filters by the member's engine (`is` carries one value, `is one of` carries many);
    // a member with no resolvable kind matches no engine filter on purpose
    if (clause.field === 'kind') {
      const kinds = ClauseIsMulti(clause) ? clause.value.values : [clause.value.value];
      return kinds.includes(rule.kind);
    }
    // the paging limit clause and unknown fields never exclude a member
    return true;
  });
}

/**
 * Decide whether a created stamp falls inside the time picker's selected range.
 *
 * This is list-domain policy rather than generic time math: under a bounded range a
 * missing (deleted/hidden or pending-fallback) or unparseable `created` excludes the
 * item, because it cannot be placed in time; the default All Time selection admits
 * everything so the picker is a pure no-op until a range is picked.
 *
 * @param created - The item's creation stamp, when one was hydrated.
 * @param time - The time picker's current selection.
 * @returns True when the item should show under the selected range.
 */
export function createdInRange(created: string | undefined, time: TimeSelection): boolean {
  // All Time filters nothing, including items with no stamp to test
  if (time.mode === 'all') {
    return true;
  }
  // a bounded range can never place an undated item, so it hides
  if (!created) {
    return false;
  }
  // an unparseable stamp is as unplaceable as a missing one
  const stamp = new Date(created).getTime();
  if (Number.isNaN(stamp)) {
    return false;
  }
  // both bounds are set for absolute/relative selections; guard anyway so a future
  // half-open mode fails open on the missing side
  const [start, end] = TimeSelectionToDateRange(time);
  return (start === null || stamp >= start.getTime()) && (end === null || stamp <= end.getTime());
}

/**
 * Filter and name-sort a ruleset's hydrated members for display.
 *
 * @param members - The hydrated member rules (undefined before the entity lands).
 * @param clauses - The omnibar clauses to filter with.
 * @param time - The time picker selection narrowing by member creation date.
 * @returns The members passing every clause and the time range, sorted by name.
 */
export function filterRuleSetMembers(members: RuleSetMember[] | undefined, clauses: Clause[], time: TimeSelection): RuleSetMember[] {
  return sortMembersList((members ?? []).filter((rule) => ruleMatchesClauses(rule, clauses) && createdInRange(rule.created, time)));
}

/**
 * Count the pages a list needs at a given page size (always at least one).
 *
 * @param total - The number of listed items.
 * @param limit - The page size.
 * @returns The number of pages.
 */
export function pageCount(total: number, limit: number): number {
  return Math.max(1, Math.ceil(total / limit));
}

/**
 * Clamp a remembered page index after the filtered set shrinks (filter edit, member removal).
 *
 * @param page - The current page index.
 * @param total - The number of listed items.
 * @param limit - The page size.
 * @returns The page index, pulled back onto the last page when it ran past the end.
 */
export function clampPage(page: number, total: number, limit: number): number {
  return Math.min(page, pageCount(total, limit) - 1);
}
