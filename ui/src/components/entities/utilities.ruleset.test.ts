import { describe, it, expect, vi } from 'vitest';

// mock the entities client so importing utilities never touches the API
vi.mock('@thorpi/entities', () => ({
  listEntities: vi.fn().mockResolvedValue({ entityList: [], entityCursor: null }),
}));

// project imports
import { buildCreateEntityForm, buildUpdateEntityForm, copyEntityFields } from './utilities';
import { Entities } from '@models/entities/entities';
import { BlankCreateRuleSet, BlankRuleSet, CreateRuleSetEntity, RuleSetEntity } from '@models/entities/rulesets';

// These tests pin the multipart field names for rule sets — they are the contract with the
// API's EntityMetadataForm/EntityMetadataUpdateForm parsers and must never drift.
describe('rule set multipart forms', () => {
  it('create form sends one metadata[ruleset_rules][] entry per member id', () => {
    const entity = structuredClone(BlankCreateRuleSet);
    entity.name = 'packers';
    entity.groups = ['corn'];
    entity.metadata.RuleSet.rules = ['11111111-1111-1111-1111-111111111111', '22222222-2222-2222-2222-222222222222'];
    const form = buildCreateEntityForm(entity);
    expect(form.get('kind')).toBe('RuleSet');
    expect(form.get('name')).toBe('packers');
    expect(form.getAll('groups[]')).toEqual(['corn']);
    expect(form.getAll('metadata[ruleset_rules][]')).toEqual([
      '11111111-1111-1111-1111-111111111111',
      '22222222-2222-2222-2222-222222222222',
    ]);
  });

  it('create form sends no member fields for an empty rule set', () => {
    const entity = structuredClone(BlankCreateRuleSet);
    entity.name = 'empty';
    entity.groups = ['corn'];
    const form = buildCreateEntityForm(entity);
    expect(form.get('kind')).toBe('RuleSet');
    expect(form.getAll('metadata[ruleset_rules][]')).toEqual([]);
  });

  it('update form diffs members into add/remove ruleset_rules fields', () => {
    const entity: RuleSetEntity = structuredClone(BlankRuleSet);
    entity.name = 'packers';
    entity.metadata.RuleSet.rules = [
      { id: '11111111-1111-1111-1111-111111111111', name: 'upx', kind: Entities.YaraRule },
      { id: '22222222-2222-2222-2222-222222222222', name: 'aspack', kind: Entities.YaraRule },
    ];
    const pending: RuleSetEntity = structuredClone(entity);
    pending.metadata.RuleSet.rules = [
      { id: '22222222-2222-2222-2222-222222222222', name: 'aspack', kind: Entities.YaraRule },
      { id: '33333333-3333-3333-3333-333333333333', name: 'themida', kind: Entities.SigmaRule },
    ];
    const form = buildUpdateEntityForm(entity, pending);
    expect(form.getAll('metadata[add_ruleset_rules][]')).toEqual(['33333333-3333-3333-3333-333333333333']);
    expect(form.getAll('metadata[remove_ruleset_rules][]')).toEqual(['11111111-1111-1111-1111-111111111111']);
  });

  it('update form sends no member fields when membership is unchanged', () => {
    const entity: RuleSetEntity = structuredClone(BlankRuleSet);
    entity.metadata.RuleSet.rules = [{ id: '11111111-1111-1111-1111-111111111111', name: 'upx', kind: Entities.YaraRule }];
    const pending: RuleSetEntity = structuredClone(entity);
    const form = buildUpdateEntityForm(entity, pending);
    expect(form.getAll('metadata[add_ruleset_rules][]')).toEqual([]);
    expect(form.getAll('metadata[remove_ruleset_rules][]')).toEqual([]);
  });

  it('copyEntityFields maps hydrated members to the create id list', () => {
    const entity: RuleSetEntity = structuredClone(BlankRuleSet);
    entity.name = 'packers';
    entity.groups = ['corn'];
    entity.metadata.RuleSet.rules = [
      { id: '11111111-1111-1111-1111-111111111111', name: 'upx', kind: Entities.YaraRule },
      { id: '22222222-2222-2222-2222-222222222222', name: 'aspack', kind: Entities.SigmaRule },
    ];
    const copied = copyEntityFields(entity, structuredClone(BlankCreateRuleSet)) as CreateRuleSetEntity;
    expect(copied.kind).toBe(Entities.RuleSet);
    expect(copied.name).toBe('packers - copy');
    expect(copied.metadata.RuleSet.rules).toEqual(['11111111-1111-1111-1111-111111111111', '22222222-2222-2222-2222-222222222222']);
  });
});
