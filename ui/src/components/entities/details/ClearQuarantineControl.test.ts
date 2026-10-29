import { describe, it, expect } from 'vitest';

// project imports
import { canClearQuarantine, clearableQuarantineValues, quarantineStatusValues } from './ClearQuarantineControl';
import { BlankYaraRule, YARA_RULE_STATUS_TAG, YaraRule } from '@models/entities/rules/yara';

/** Build a yara rule with the given id and status tag values on top of the blank defaults. */
function ruleWith(id: string, statusValues: string[] = []): YaraRule {
  const rule = structuredClone(BlankYaraRule);
  rule.id = id;
  if (statusValues.length > 0) {
    // entity tags map each value to the groups that can see it
    rule.tags[YARA_RULE_STATUS_TAG] = Object.fromEntries(statusValues.map((value) => [value, ['corn']]));
  }
  return rule;
}

const RULE_ID = '0a658ba7-78a9-4a2e-8df3-7fbf18960286';

describe('quarantineStatusValues', () => {
  it('reads the YaraRuleStatus values sorted for stable display', () => {
    expect(quarantineStatusValues(ruleWith(RULE_ID, ['Slow', 'Noisy']))).toEqual(['Noisy', 'Slow']);
  });

  it('returns an empty list for an unquarantined rule', () => {
    expect(quarantineStatusValues(ruleWith(RULE_ID))).toEqual([]);
  });

  it('ignores other tag keys', () => {
    const rule = ruleWith(RULE_ID);
    rule.tags['YaraRuleHash'] = { abc123: ['corn'] };
    expect(quarantineStatusValues(rule)).toEqual([]);
  });
});

describe('clearableQuarantineValues', () => {
  it('keeps only the Noisy/Slow values a clear may delete', () => {
    expect(clearableQuarantineValues(ruleWith(RULE_ID, ['Broken', 'Slow', 'Noisy']))).toEqual(['Noisy', 'Slow']);
  });

  it('leaves Broken alone since only a source fix clears it', () => {
    expect(clearableQuarantineValues(ruleWith(RULE_ID, ['Broken']))).toEqual([]);
  });
});

describe('canClearQuarantine', () => {
  it('allows a loaded rule that carries a clearable status value', () => {
    expect(canClearQuarantine(ruleWith(RULE_ID, ['Noisy']))).toBe(true);
    expect(canClearQuarantine(ruleWith(RULE_ID, ['Broken', 'Slow']))).toBe(true);
  });

  it('denies a rule whose only status is Broken', () => {
    // clearing Broken would just re-tag and re-notify on the next scanner restart
    expect(canClearQuarantine(ruleWith(RULE_ID, ['Broken']))).toBe(false);
  });

  it('denies an unquarantined rule', () => {
    expect(canClearQuarantine(ruleWith(RULE_ID))).toBe(false);
  });

  it('denies the blank placeholder even when it somehow carries the tag', () => {
    // a missing or mismatched route id leaves BlankYaraRule in place; clearing it would
    // DELETE /entities/tags/ with an empty id
    expect(canClearQuarantine(ruleWith('', ['Noisy']))).toBe(false);
  });
});
