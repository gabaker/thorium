import { describe, it, expect } from 'vitest';

// project imports
import {
  clampPage,
  createdInRange,
  filterRuleSetMembers,
  memberDetailsUrl,
  pageCount,
  ruleMatchesClauses,
  sortMembersList,
} from './ruleSetRules';
import { Clause, ClauseCondition } from '@components/shared/inputs/omnibar/ClauseTypes';
import { TimeSelection } from '@components/shared/inputs/omnibar/timepicker/utils';
import { Entities } from '@models/entities';
import { RuleSetMember } from '@models/entities/rulesets';

// hydrated members as the API serves them (members may mix engines)
const sigmaMember: RuleSetMember = { id: 'sig-1', name: 'suspicious-eventlog', kind: Entities.SigmaRule, created: '2026-10-01T00:00:00Z' };
const yaraMember: RuleSetMember = { id: 'yar-1', name: 'packed-pe', kind: Entities.YaraRule, created: '2026-10-17T00:00:00Z' };
const members = [yaraMember, sigmaMember];

// the picker's default selection: filters nothing
const allTime: TimeSelection = { mode: 'all' };
// an absolute range holding only the yara member's created stamp
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

describe('memberDetailsUrl', () => {
  it('links a yara member to the yara details route', () => {
    expect(memberDetailsUrl(yaraMember)).toBe('/rules/yara/yar-1');
  });

  it('links a sigma member to the sigma details route', () => {
    expect(memberDetailsUrl(sigmaMember)).toBe('/rules/sigma/sig-1');
  });

  it('returns undefined for a kind this build cannot route', () => {
    // a newer server can serve member kinds added after this UI shipped (rolling upgrades)
    const futureMember: RuleSetMember = { id: 'snort-1', name: 'future-rule', kind: 'SnortRule' as Entities };
    expect(memberDetailsUrl(futureMember)).toBeUndefined();
  });
});

describe('sortMembersList', () => {
  it('sorts a copy by name without touching the input array', () => {
    const input = [sigmaMember, yaraMember];
    const sorted = sortMembersList(input);
    expect(sorted).toEqual([yaraMember, sigmaMember]);
    // the input array (React state in the callers) must never be reordered in place
    expect(input).toEqual([sigmaMember, yaraMember]);
  });
});

describe('ruleMatchesClauses', () => {
  it('matches free text case-insensitively on a name substring', () => {
    expect(ruleMatchesClauses(yaraMember, [clause('text', 'PACK')])).toBe(true);
    expect(ruleMatchesClauses(sigmaMember, [clause('text', 'PACK')])).toBe(false);
  });

  it('ANDs multiple text clauses together', () => {
    expect(ruleMatchesClauses(yaraMember, [clause('text', 'packed'), clause('text', 'pe')])).toBe(true);
    expect(ruleMatchesClauses(yaraMember, [clause('text', 'packed'), clause('text', 'eventlog')])).toBe(false);
  });

  it('matches the member id when the name is missing at runtime', () => {
    // a pending fallback member carries only its id as the name source
    const nameless = { id: 'mystery-7' } as RuleSetMember;
    expect(ruleMatchesClauses(nameless, [clause('text', 'mystery')])).toBe(true);
    expect(ruleMatchesClauses(nameless, [clause('text', 'packed')])).toBe(false);
  });

  it('filters by engine with a single kind clause', () => {
    expect(ruleMatchesClauses(sigmaMember, [clause('kind', Entities.SigmaRule)])).toBe(true);
    expect(ruleMatchesClauses(yaraMember, [clause('kind', Entities.SigmaRule)])).toBe(false);
  });

  it('filters by engine with an is-one-of kind clause', () => {
    const both = [multiClause('kind', [Entities.SigmaRule, Entities.YaraRule])];
    expect(ruleMatchesClauses(sigmaMember, both)).toBe(true);
    expect(ruleMatchesClauses(yaraMember, both)).toBe(true);
    expect(ruleMatchesClauses(yaraMember, [multiClause('kind', [Entities.SigmaRule])])).toBe(false);
  });

  it('never excludes a member on limit or unknown clauses', () => {
    expect(ruleMatchesClauses(yaraMember, [clause('limit', '10')])).toBe(true);
    expect(ruleMatchesClauses(yaraMember, [clause('bogus', 'anything')])).toBe(true);
  });
});

describe('createdInRange', () => {
  it('admits everything under the default All Time selection', () => {
    expect(createdInRange('2026-10-17T00:00:00Z', allTime)).toBe(true);
    // undated items (pending fallbacks, deleted frozen lines) also pass
    expect(createdInRange(undefined, allTime)).toBe(true);
  });

  it('tests an absolute range inclusively on both bounds', () => {
    expect(createdInRange('2026-10-17T00:00:00Z', octoberTail)).toBe(true);
    expect(createdInRange('2026-10-10T00:00:00Z', octoberTail)).toBe(true);
    expect(createdInRange('2026-10-20T00:00:00Z', octoberTail)).toBe(true);
    expect(createdInRange('2026-10-01T00:00:00Z', octoberTail)).toBe(false);
    expect(createdInRange('2026-10-21T00:00:00Z', octoberTail)).toBe(false);
  });

  it('hides undated and unparseable items under a bounded range', () => {
    // a bounded range can never place an item with no usable stamp
    expect(createdInRange(undefined, octoberTail)).toBe(false);
    expect(createdInRange('not-a-date', octoberTail)).toBe(false);
  });

  it('resolves a relative selection against now', () => {
    // a century-wide window avoids clock mocking while still exercising the relative path
    const lastCentury: TimeSelection = { mode: 'relative', amount: 100, unit: 'year' };
    expect(createdInRange('2026-10-17T00:00:00Z', lastCentury)).toBe(true);
    expect(createdInRange('1901-01-01T00:00:00Z', lastCentury)).toBe(false);
  });
});

describe('filterRuleSetMembers', () => {
  it('returns every member name-sorted when no clauses are set', () => {
    expect(filterRuleSetMembers(members, [], allTime)).toEqual([yaraMember, sigmaMember]);
  });

  it('returns an empty list for undefined members', () => {
    // the entity may not have landed yet when the section first renders
    expect(filterRuleSetMembers(undefined, [], allTime)).toEqual([]);
  });

  it('combines a text and a kind clause', () => {
    expect(filterRuleSetMembers(members, [clause('text', 'e'), clause('kind', Entities.YaraRule)], allTime)).toEqual([yaraMember]);
    expect(filterRuleSetMembers(members, [clause('text', 'eventlog'), clause('kind', Entities.YaraRule)], allTime)).toEqual([]);
  });

  it('narrows by the time range alongside the clauses', () => {
    expect(filterRuleSetMembers(members, [], octoberTail)).toEqual([yaraMember]);
    expect(filterRuleSetMembers(members, [clause('kind', Entities.SigmaRule)], octoberTail)).toEqual([]);
  });
});

describe('pageCount', () => {
  it('rounds partial pages up', () => {
    expect(pageCount(26, 25)).toBe(2);
  });

  it('does not add a page on an exact multiple', () => {
    expect(pageCount(50, 25)).toBe(2);
  });

  it('always reports at least one page', () => {
    expect(pageCount(0, 25)).toBe(1);
  });
});

describe('clampPage', () => {
  it('keeps an in-range page untouched', () => {
    expect(clampPage(1, 50, 25)).toBe(1);
  });

  it('pulls a page past the end back onto the last page', () => {
    // a filter edit or member removal shrank the set while page 3 was remembered
    expect(clampPage(3, 30, 25)).toBe(1);
  });

  it('clamps to page zero when nothing matches', () => {
    expect(clampPage(3, 0, 25)).toBe(0);
  });
});
