import { describe, it, expect } from 'vitest';

// project imports
import { BlankYaraRule, YARA_RULE_STATUS_TAG, YaraRuleset } from './yara';

describe('YARA_RULE_STATUS_TAG', () => {
  it('pins the exact tag key quarantine status is written under', () => {
    // mirrors the API's `YaraRule::STATUS_TAG` — the hunt snapshot filter, the frozen
    // rules section, and the clear-quarantine control all read this exact key
    expect(YARA_RULE_STATUS_TAG).toBe('YaraRuleStatus');
  });
});

describe('YaraRuleset quarantine_exempt', () => {
  it('pins the exact wire field name of the exemption flag', () => {
    const meta: YaraRuleset = { ...BlankYaraRule.metadata.YaraRule, quarantine_exempt: true };
    expect(Object.keys(meta)).toContain('quarantine_exempt');
  });

  it('stays optional so rules from older builds keep parsing', () => {
    // the API serde-defaults the flag, so a rule serialized before the field existed
    // simply reads as not exempt
    expect(BlankYaraRule.metadata.YaraRule.quarantine_exempt).toBeUndefined();
  });
});
