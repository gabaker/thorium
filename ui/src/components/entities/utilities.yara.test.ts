import { describe, it, expect } from 'vitest';

// project imports
import { buildCreateEntityForm, buildUpdateEntityForm } from './utilities';
import { BlankCreateYaraRule, BlankYaraRule, YaraRule, YaraRuleAppliesTo } from '@models/entities/rules/yara';

// These tests pin the multipart field names for yara rules — they are the contract with the
// API's EntityMetadataForm/EntityMetadataUpdateForm parsers and must never drift.
describe('yara rule multipart forms', () => {
  it('create form uses the yara metadata field names', () => {
    const entity = structuredClone(BlankCreateYaraRule);
    entity.name = 'upx-packed';
    entity.groups = ['corn'];
    entity.metadata.YaraRule.rule = 'rule upx { condition: true }';
    entity.metadata.YaraRule.score = 20;
    entity.metadata.YaraRule.applies_to = [YaraRuleAppliesTo.Samples];
    entity.metadata.YaraRule.actions = [{ Flag: { content: 'packed', reasoning: 'upx packer found' } }];
    const form = buildCreateEntityForm(entity);
    expect(form.get('kind')).toBe('YaraRule');
    expect(form.get('metadata[yara_rule]')).toBe('rule upx { condition: true }');
    expect(form.get('metadata[score]')).toBe('20');
    expect(form.getAll('metadata[yara_applies_to][]')).toEqual([YaraRuleAppliesTo.Samples]);
    expect(form.getAll('metadata[yara_actions][]')).toEqual([
      JSON.stringify({ Flag: { content: 'packed', reasoning: 'upx packer found' } }),
    ]);
  });

  it('update form uses the yara add/remove field names', () => {
    const entity: YaraRule = structuredClone(BlankYaraRule);
    entity.name = 'upx-packed';
    entity.metadata.YaraRule.rule = 'rule upx { condition: true }';
    entity.metadata.YaraRule.applies_to = [];
    entity.metadata.YaraRule.actions = [{ Flag: { reasoning: 'old action' } }];
    const pending: YaraRule = structuredClone(entity);
    pending.metadata.YaraRule.rule = 'rule upx { condition: false }';
    pending.metadata.YaraRule.score = 50;
    pending.metadata.YaraRule.applies_to = [YaraRuleAppliesTo.Samples];
    pending.metadata.YaraRule.actions = [];
    const form = buildUpdateEntityForm(entity, pending);
    expect(form.get('metadata[yara_rule]')).toBe('rule upx { condition: false }');
    expect(form.get('metadata[score]')).toBe('50');
    expect(form.getAll('metadata[add_yara_applies_to][]')).toEqual([YaraRuleAppliesTo.Samples]);
    expect(form.getAll('metadata[remove_yara_actions][]')).toEqual(['0']);
  });
});
