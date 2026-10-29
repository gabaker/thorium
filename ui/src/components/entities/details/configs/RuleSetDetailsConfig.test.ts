import { describe, it, expect } from 'vitest';

// project imports
import RuleSetDetailsConfig, { resolvePendingMembers } from './RuleSetDetailsConfig';
import HuntDetailsConfig from './HuntDetailsConfig';
import VendorDetailsConfig from './VendorDetailsConfig';
import { memberDetailsUrl } from './ruleSetRules';
import { Entities } from '@models/entities';
import { RuleSetMember } from '@models/entities/rulesets';

// hydrated members as the API serves them (members may mix engines)
const sigmaMember: RuleSetMember = { id: 'sig-1', name: 'suspicious-eventlog', kind: Entities.SigmaRule };
const yaraMember: RuleSetMember = { id: 'yar-1', name: 'packed-pe', kind: Entities.YaraRule };

describe('RuleSetDetailsConfig', () => {
  it('replaces the associations graph with the member rules section', () => {
    expect(RuleSetDetailsConfig.AssociationsSection).toBeDefined();
  });

  it('replaces the associations graph with the frozen rules section on hunts', () => {
    expect(HuntDetailsConfig.AssociationsSection).toBeDefined();
  });

  it('leaves the default associations graph on other kinds', () => {
    // pin that the replacement stays ruleset+hunt only instead of creeping across kinds
    expect(VendorDetailsConfig.AssociationsSection).toBeUndefined();
  });
});

describe('resolvePendingMembers', () => {
  it('preserves known member objects with their real kinds', () => {
    const resolved = resolvePendingMembers(['sig-1', 'yar-1'], [sigmaMember, yaraMember], {}, {});
    expect(resolved).toEqual([sigmaMember, yaraMember]);
    // the sigma member must still link to the sigma details page when a save publishes
    // the pending entity straight into read mode (the original stubbing bug)
    expect(memberDetailsUrl(resolved[0])).toBe('/rules/sigma/sig-1');
  });

  it('resolves a newly added id from the picker listing maps', () => {
    const resolved = resolvePendingMembers(['sig-1', 'sig-2'], [sigmaMember], { 'sig-2': 'new-sigma' }, { 'sig-2': Entities.SigmaRule });
    expect(resolved).toEqual([sigmaMember, { id: 'sig-2', name: 'new-sigma', kind: Entities.SigmaRule }]);
    expect(memberDetailsUrl(resolved[1])).toBe('/rules/sigma/sig-2');
  });

  it('drops members deselected in the picker', () => {
    const resolved = resolvePendingMembers(['yar-1'], [sigmaMember, yaraMember], {}, {});
    expect(resolved).toEqual([yaraMember]);
  });

  it('falls back linkless for an id neither the members nor the maps know', () => {
    const resolved = resolvePendingMembers(['mystery'], [], {}, {});
    expect(resolved).toHaveLength(1);
    // the name falls back to the id while the picker maps are still loading
    expect(resolved[0].id).toBe('mystery');
    expect(resolved[0].name).toBe('mystery');
    // no kind was resolvable, so the member must never render a kind-specific link
    expect(memberDetailsUrl(resolved[0])).toBeUndefined();
  });

  it('names a known-kind addition by its id when the name map missed it', () => {
    const resolved = resolvePendingMembers(['yar-2'], [], {}, { 'yar-2': Entities.YaraRule });
    expect(resolved).toEqual([{ id: 'yar-2', name: 'yar-2', kind: Entities.YaraRule }]);
  });
});
