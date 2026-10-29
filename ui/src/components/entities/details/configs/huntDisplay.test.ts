import { describe, it, expect } from 'vitest';

// project imports
import { describeSelector, huntBarClass, huntStatusVariant, laneBarClass, selectorRuleSetId } from './huntDisplay';
import { HuntLane, HuntStatus } from '@models/entities/hunts';

describe('huntStatusVariant', () => {
  it('maps every status to its badge variant', () => {
    expect(huntStatusVariant(HuntStatus.Created)).toBe('secondary');
    expect(huntStatusVariant(HuntStatus.Running)).toBe('primary');
    expect(huntStatusVariant(HuntStatus.Completed)).toBe('success');
    expect(huntStatusVariant(HuntStatus.Cancelled)).toBe('warning');
    expect(huntStatusVariant(HuntStatus.Failed)).toBe('danger');
  });
});

describe('huntBarClass', () => {
  it('maps every status to its themed bar class', () => {
    expect(huntBarClass(HuntStatus.Created)).toBe('info-bar');
    expect(huntBarClass(HuntStatus.Running)).toBe('info-bar');
    expect(huntBarClass(HuntStatus.Completed)).toBe('success-bar');
    expect(huntBarClass(HuntStatus.Cancelled)).toBe('warning-bar');
    expect(huntBarClass(HuntStatus.Failed)).toBe('danger-bar');
  });
});

describe('laneBarClass', () => {
  // a minimal lane shaped like the model; only exhausted matters here
  const lane = (exhausted: boolean): HuntLane => ({ start: '', current: '', end: '', seq: 0, cursor: null, exhausted });

  it('draws an exhausted lane green regardless of hunt status', () => {
    expect(laneBarClass(HuntStatus.Running, lane(true))).toBe('success-bar');
    expect(laneBarClass(HuntStatus.Cancelled, lane(true))).toBe('success-bar');
    expect(laneBarClass(HuntStatus.Failed, lane(true))).toBe('success-bar');
  });

  it('follows the hunt status for an unexhausted lane', () => {
    expect(laneBarClass(HuntStatus.Created, lane(false))).toBe('info-bar');
    expect(laneBarClass(HuntStatus.Running, lane(false))).toBe('info-bar');
    expect(laneBarClass(HuntStatus.Cancelled, lane(false))).toBe('warning-bar');
    expect(laneBarClass(HuntStatus.Failed, lane(false))).toBe('danger-bar');
  });
});

describe('describeSelector', () => {
  it('describes the bare-string All variant', () => {
    expect(describeSelector('All')).toBe('All rules visible at creation');
  });

  it('describes explicit rules with their count', () => {
    expect(describeSelector({ Rules: ['a', 'b'] })).toBe('Explicit rules (2)');
  });

  it('describes a tag selector with its pair', () => {
    expect(describeSelector({ Tag: { key: 'family', value: 'emotet' } })).toBe('Rules tagged family = emotet');
  });

  it('describes a rule set selector', () => {
    expect(describeSelector({ RuleSet: 'rs1' })).toBe('Rule set');
  });
});

describe('selectorRuleSetId', () => {
  it('returns the id only for the RuleSet variant', () => {
    expect(selectorRuleSetId({ RuleSet: 'rs1' })).toBe('rs1');
    expect(selectorRuleSetId('All')).toBeNull();
    expect(selectorRuleSetId({ Rules: ['a'] })).toBeNull();
    expect(selectorRuleSetId({ Tag: { key: 'k', value: 'v' } })).toBeNull();
  });
});
