import { describe, it, expect } from 'vitest';

// project imports
import {
  BlankHuntCreateFormState,
  buildHuntCreateRequest,
  describeCorpusBounds,
  HuntSelectorMode,
  huntToFormState,
  validateHuntForm,
} from './huntCreateRequest';
import { BlankHunt, Hunt, HuntEngine, HuntSelector } from '@models/entities/hunts';

// These tests pin the JSON wire shapes for POST /entities/hunt/ — the contract with the
// API's HuntCreateRequest serde and must never drift.
describe('buildHuntCreateRequest', () => {
  it('builds a deduped Rules selector and omits the untouched corpus and empty description', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.name = 'hunt-upx';
    state.groups = ['corn'];
    state.selectorMode = HuntSelectorMode.Rules;
    state.selectorRules = ['u1', 'u2', 'u1'];
    const request = buildHuntCreateRequest(state);
    expect(JSON.stringify(request)).toBe(
      JSON.stringify({ name: 'hunt-upx', groups: ['corn'], engine: 'yara', selector: { Rules: ['u1', 'u2'] } }),
    );
  });

  it('builds the Tag selector shape', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.selectorMode = HuntSelectorMode.Tag;
    state.selectorTagKey = 'family';
    state.selectorTagValue = 'emotet';
    expect(buildHuntCreateRequest(state).selector).toEqual({ Tag: { key: 'family', value: 'emotet' } });
  });

  it('trims the Tag selector pair like every other tag input', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.selectorMode = HuntSelectorMode.Tag;
    state.selectorTagKey = 'family ';
    state.selectorTagValue = ' emotet';
    expect(buildHuntCreateRequest(state).selector).toEqual({ Tag: { key: 'family', value: 'emotet' } });
  });

  it('builds the All selector as a bare string', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.selectorMode = HuntSelectorMode.All;
    expect(buildHuntCreateRequest(state).selector).toBe('All');
  });

  it('builds the RuleSet selector shape', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.selectorMode = HuntSelectorMode.RuleSet;
    state.selectorRuleSet = 'rs1';
    expect(buildHuntCreateRequest(state).selector).toEqual({ RuleSet: 'rs1' });
  });

  it('sends only the corpus bounds that were set', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.corpusEnd = '2026-01-01T00:00:00.000Z';
    const request = buildHuntCreateRequest(state);
    expect(request.corpus).toEqual({ Files: { end: '2026-01-01T00:00:00.000Z' } });
  });

  it('never posts a corpus group outside the hunt groups', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.groups = ['corn'];
    state.corpusGroups = ['corn', 'wheat'];
    expect(buildHuntCreateRequest(state).corpus).toEqual({ Files: { groups: ['corn'] } });
    // when every corpus group went stale the whole bound is omitted
    state.corpusGroups = ['wheat'];
    expect(buildHuntCreateRequest(state).corpus).toBeUndefined();
  });

  it('folds corpus tag rows into the strict-AND map, skipping blanks and duplicates', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.corpusTags = [
      { key: 'family', value: 'emotet' },
      { key: 'family', value: 'qakbot' },
      { key: 'family', value: 'emotet' },
      { key: '', value: 'ignored' },
      { key: 'ignored', value: '' },
    ];
    state.corpusCaseInsensitive = true;
    const request = buildHuntCreateRequest(state);
    expect(request.corpus).toEqual({
      Files: { tags: { family: ['emotet', 'qakbot'] }, tags_case_insensitive: true },
    });
  });

  it('folds tag keys that collide with Object.prototype names', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.corpusTags = [
      { key: 'toString', value: 'v1' },
      { key: 'constructor', value: 'v2' },
    ];
    expect(buildHuntCreateRequest(state).corpus).toEqual({
      Files: { tags: { toString: ['v1'], constructor: ['v2'] } },
    });
  });

  it('omits the case-insensitive flag when no tag filters survive', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.corpusCaseInsensitive = true;
    expect(buildHuntCreateRequest(state).corpus).toBeUndefined();
  });

  it('passes the engine through and includes a set description', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.engine = HuntEngine.Yara;
    state.description = 'cover history';
    const request = buildHuntCreateRequest(state);
    expect(request.engine).toBe('yara');
    expect(request.description).toBe('cover history');
  });
});

describe('huntToFormState', () => {
  it('maps every selector variant back onto its form mode', () => {
    const hunt: Hunt = structuredClone(BlankHunt);
    hunt.name = 'old-hunt';
    hunt.groups = ['corn'];
    hunt.metadata.Hunt.selector = { Rules: ['u1'] };
    expect(huntToFormState(hunt).selectorMode).toBe(HuntSelectorMode.Rules);
    expect(huntToFormState(hunt).selectorRules).toEqual(['u1']);
    hunt.metadata.Hunt.selector = { Tag: { key: 'k', value: 'v' } };
    expect(huntToFormState(hunt).selectorTagKey).toBe('k');
    hunt.metadata.Hunt.selector = 'All';
    expect(huntToFormState(hunt).selectorMode).toBe(HuntSelectorMode.All);
    hunt.metadata.Hunt.selector = { RuleSet: 'rs1' };
    expect(huntToFormState(hunt).selectorRuleSet).toBe('rs1');
  });

  it('copies the corpus bounds and marks the name as a rerun', () => {
    const hunt: Hunt = structuredClone(BlankHunt);
    hunt.name = 'old-hunt';
    hunt.metadata.Hunt.corpus = {
      Files: {
        groups: ['corn'],
        start: '2026-02-01T00:00:00.000Z',
        end: '2026-01-01T00:00:00.000Z',
        tags: { family: ['emotet'] },
        tags_case_insensitive: true,
      },
    };
    const state = huntToFormState(hunt);
    expect(state.name).toBe('old-hunt - rerun');
    expect(state.corpusGroups).toEqual(['corn']);
    expect(state.corpusStart).toBe('2026-02-01T00:00:00.000Z');
    expect(state.corpusEnd).toBe('2026-01-01T00:00:00.000Z');
    expect(state.corpusTags).toEqual([{ key: 'family', value: 'emotet' }]);
    expect(state.corpusCaseInsensitive).toBe(true);
  });

  it('normalizes an unknown engine to the default and drops its engine-scoped picks', () => {
    // a newer server can serve engines this build does not know; copying one must prefill
    // the default engine with no stale foreign rule picks instead of crashing the page
    const hunt: Hunt = structuredClone(BlankHunt);
    hunt.metadata.Hunt.engine = 'sigma' as unknown as HuntEngine;
    hunt.metadata.Hunt.selector = { Rules: ['foreign-rule'] };
    const state = huntToFormState(hunt);
    expect(state.engine).toBe(HuntEngine.Yara);
    expect(state.selectorRules).toEqual([]);
    hunt.metadata.Hunt.selector = { RuleSet: 'foreign-set' };
    expect(huntToFormState(hunt).selectorRuleSet).toBe('');
  });

  it('falls back to the All mode for a selector variant this build does not know', () => {
    const hunt: Hunt = structuredClone(BlankHunt);
    hunt.metadata.Hunt.selector = { Frobnicate: 'x' } as unknown as HuntSelector;
    const state = huntToFormState(hunt);
    expect(state.selectorMode).toBe(HuntSelectorMode.All);
    expect(state.selectorRuleSet).toBe('');
    // the fallback round-trips to a selector that is always valid on the wire
    expect(buildHuntCreateRequest(state).selector).toBe('All');
  });
});

describe('validateHuntForm', () => {
  it('passes the blank form and the valid selector shapes', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    expect(validateHuntForm(state)).toBe('');
    state.selectorMode = HuntSelectorMode.Tag;
    state.selectorTagKey = 'family';
    expect(validateHuntForm(state)).toBe('');
    state.selectorMode = HuntSelectorMode.RuleSet;
    state.selectorRuleSet = 'rs1';
    expect(validateHuntForm(state)).toBe('');
  });

  it('blocks a Tag selector whose key is empty after trimming', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.selectorMode = HuntSelectorMode.Tag;
    state.selectorTagKey = '   ';
    expect(validateHuntForm(state)).not.toBe('');
  });

  it('blocks a RuleSet selector with nothing picked', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.selectorMode = HuntSelectorMode.RuleSet;
    expect(validateHuntForm(state)).not.toBe('');
    // a copied future-variant selector could only ever leave the pick undefined
    state.selectorRuleSet = undefined as unknown as string;
    expect(validateHuntForm(state)).not.toBe('');
  });
});

describe('describeCorpusBounds', () => {
  it('stays silent while the server defaults apply', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    expect(describeCorpusBounds(state)).toBe('');
    // blank tag rows and the lone case flag post nothing, so they describe nothing
    state.corpusTags = [{ key: ' ', value: '' }];
    state.corpusCaseInsensitive = true;
    expect(describeCorpusBounds(state)).toBe('');
  });

  it('surfaces every bound the form would post', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.corpusGroups = ['corn', 'wheat'];
    state.corpusStart = '2026-02-01T00:00:00.000Z';
    state.corpusEnd = '2026-01-01T00:00:00.000Z';
    state.corpusTags = [
      { key: 'family', value: 'emotet' },
      { key: 'family', value: 'qakbot' },
    ];
    state.corpusCaseInsensitive = true;
    const summary = describeCorpusBounds(state);
    expect(summary).toContain('groups: corn, wheat');
    expect(summary).toContain(`newest: ${new Date('2026-02-01T00:00:00.000Z').toLocaleString()}`);
    expect(summary).toContain(`oldest: ${new Date('2026-01-01T00:00:00.000Z').toLocaleString()}`);
    expect(summary).toContain('2 tag filters (case-insensitive)');
  });

  it('keeps an unparseable bound visible verbatim', () => {
    const state = structuredClone(BlankHuntCreateFormState);
    state.corpusStart = 'not-a-date';
    expect(describeCorpusBounds(state)).toContain('newest: not-a-date');
  });
});
