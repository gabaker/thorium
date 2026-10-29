import { describe, it, expect } from 'vitest';

// project imports
import {
  clampPage,
  createdInRange,
  filterFrozenRules,
  frozenRuleDetailsUrl,
  frozenRuleMatchesClauses,
  pageCount,
  sortFrozenRules,
} from './huntFrozenRules';
import { Clause, ClauseCondition } from '@components/shared/inputs/omnibar/ClauseTypes';
import { TimeSelection } from '@components/shared/inputs/omnibar/timepicker/utils';
import { Entities } from '@models/entities';
import { HuntRuleLine } from '@models/entities/hunts';

// frozen rule lines as the thorpi wrapper normalizes them (status/exempt always present)
const hydrated: HuntRuleLine = {
  id: '0a658ba7-78a9-4a2e-8df3-7fbf18960286',
  name: 'packed-pe',
  kind: Entities.YaraRule,
  created: '2026-10-17T00:00:00Z',
  status: ['Noisy'],
  quarantine_exempt: false,
};
// a rule deleted after the freeze (or hidden from the caller) hydrates nothing
const idOnly: HuntRuleLine = { id: 'bb-mystery-7', status: [], quarantine_exempt: false };
const lines = [idOnly, hydrated];

// the picker's default selection: filters nothing
const allTime: TimeSelection = { mode: 'all' };
// an absolute range holding the hydrated line's created stamp
const octoberTail: TimeSelection = { mode: 'absolute', start: new Date('2026-10-10T00:00:00Z'), end: new Date('2026-10-20T00:00:00Z') };

// build a single-value clause the way the omnibar emits them
const clause = (field: string, value: string, category: string = field): Clause => ({
  category,
  field,
  condition: ClauseCondition.Is,
  value: { value },
});

// build a multi-value `is one of` clause
const multiClause = (field: string, values: string[], category: string = field): Clause => ({
  category,
  field,
  condition: ClauseCondition.IsOneOf,
  value: { values },
});

describe('frozenRuleDetailsUrl', () => {
  it('links a hydrated yara line to the yara details route', () => {
    expect(frozenRuleDetailsUrl(hydrated)).toBe('/rules/yara/0a658ba7-78a9-4a2e-8df3-7fbf18960286');
  });

  it('returns undefined for an id-only deleted or hidden line', () => {
    // no kind hydrated means no engine to route to — the row must render linkless
    expect(frozenRuleDetailsUrl(idOnly)).toBeUndefined();
  });

  it('returns undefined for a kind this build cannot route', () => {
    // a newer server can serve rule kinds added after this UI shipped (rolling upgrades)
    const future: HuntRuleLine = { ...hydrated, kind: 'SnortRule' as Entities };
    expect(frozenRuleDetailsUrl(future)).toBeUndefined();
  });
});

describe('sortFrozenRules', () => {
  it('sorts a copy by name with the id as fallback, without touching the input', () => {
    const input = [idOnly, hydrated];
    const sorted = sortFrozenRules(input);
    // 'bb-mystery-7' (the id fallback) sorts before 'packed-pe'
    expect(sorted).toEqual([idOnly, hydrated]);
    expect(sortFrozenRules([hydrated, idOnly])).toEqual([idOnly, hydrated]);
    // the input array (React state in the section) must never be reordered in place
    expect(input).toEqual([idOnly, hydrated]);
  });
});

describe('frozenRuleMatchesClauses', () => {
  it('matches free text case-insensitively on a name substring', () => {
    expect(frozenRuleMatchesClauses(hydrated, [clause('text', 'PACK')])).toBe(true);
    expect(frozenRuleMatchesClauses(hydrated, [clause('text', 'eventlog')])).toBe(false);
  });

  it('matches free text on the id so deleted or hidden lines stay findable', () => {
    expect(frozenRuleMatchesClauses(idOnly, [clause('text', 'MYSTERY')])).toBe(true);
    // a hydrated line also matches on its id, mirroring what the row can display
    expect(frozenRuleMatchesClauses(hydrated, [clause('text', '0a658ba7')])).toBe(true);
    expect(frozenRuleMatchesClauses(idOnly, [clause('text', 'packed')])).toBe(false);
  });

  it('filters by engine with single and is-one-of kind clauses', () => {
    expect(frozenRuleMatchesClauses(hydrated, [clause('kind', Entities.YaraRule)])).toBe(true);
    expect(frozenRuleMatchesClauses(hydrated, [clause('kind', Entities.SigmaRule)])).toBe(false);
    expect(frozenRuleMatchesClauses(hydrated, [multiClause('kind', [Entities.SigmaRule, Entities.YaraRule])])).toBe(true);
  });

  it('matches no engine filter for a line with no hydrated kind', () => {
    expect(frozenRuleMatchesClauses(idOnly, [clause('kind', Entities.YaraRule)])).toBe(false);
  });

  it('never excludes a line on limit or unknown clauses', () => {
    expect(frozenRuleMatchesClauses(hydrated, [clause('limit', '10')])).toBe(true);
    expect(frozenRuleMatchesClauses(idOnly, [clause('limit', '10')])).toBe(true);
    expect(frozenRuleMatchesClauses(hydrated, [clause('bogus', 'anything')])).toBe(true);
  });
});

describe('filterFrozenRules', () => {
  it('returns every line name-sorted when no clauses are set', () => {
    expect(filterFrozenRules(lines, [], allTime)).toEqual([idOnly, hydrated]);
  });

  it('combines a text and a kind clause', () => {
    expect(filterFrozenRules(lines, [clause('text', 'e'), clause('kind', Entities.YaraRule)], allTime)).toEqual([hydrated]);
    expect(filterFrozenRules(lines, [clause('text', 'mystery'), clause('kind', Entities.YaraRule)], allTime)).toEqual([]);
  });

  it('hides undated deleted lines under a bounded range and keeps them under All Time', () => {
    // the deleted idOnly line hydrates no created stamp, so a bounded range cannot place it
    expect(filterFrozenRules(lines, [], octoberTail)).toEqual([hydrated]);
    expect(filterFrozenRules(lines, [], allTime)).toEqual([idOnly, hydrated]);
  });

  it('narrows hydrated lines by their created stamp', () => {
    const earlyRange: TimeSelection = { mode: 'absolute', start: new Date('2026-09-01T00:00:00Z'), end: new Date('2026-09-30T00:00:00Z') };
    expect(filterFrozenRules(lines, [], earlyRange)).toEqual([]);
  });
});

describe('re-exported paging math and time predicate', () => {
  it('shares the ruleset section page count', () => {
    expect(pageCount(30, 25)).toBe(2);
    expect(pageCount(0, 25)).toBe(1);
  });

  it('clamps a remembered page back onto the last page after a filter shrink', () => {
    expect(clampPage(3, 30, 25)).toBe(1);
    expect(clampPage(0, 30, 25)).toBe(0);
  });

  it('shares the ruleset section created-in-range predicate', () => {
    expect(createdInRange(hydrated.created, octoberTail)).toBe(true);
    expect(createdInRange(undefined, octoberTail)).toBe(false);
  });
});
