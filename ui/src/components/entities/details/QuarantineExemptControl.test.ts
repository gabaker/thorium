import { describe, it, expect } from 'vitest';

// project imports
import { canManageQuarantineExemption } from './QuarantineExemptControl';
import { BlankYaraRule, YaraRule } from '@models/entities/rules/yara';
import { ThoriumRole, UserInfo } from '@models/users';

/** Build a yara rule with the given id on top of the blank defaults. */
function ruleWith(id: string): YaraRule {
  const rule = structuredClone(BlankYaraRule);
  rule.id = id;
  return rule;
}

/** Build a minimal UserInfo with the given Thorium role. */
function makeUser(role: ThoriumRole): UserInfo {
  return {
    username: 'corn',
    role,
    email: 'corn@example.com',
    groups: [],
    token: 't',
    token_expiration: '2099-01-01T00:00:00Z',
    settings: { theme: 'Dark' },
    local: true,
    verified: true,
    has_image: false,
  };
}

// runtime role values as the API serializes them (plain strings except Developer)
const ADMIN = 'Admin' as unknown as ThoriumRole;
const USER = 'User' as unknown as ThoriumRole;
const DEV = { Developer: { k8s: true, bare_metal: false, windows: false, external: false, kvm: false } } as unknown as ThoriumRole;

const loadedRule = ruleWith('0a658ba7-78a9-4a2e-8df3-7fbf18960286');

describe('canManageQuarantineExemption', () => {
  it('allows an admin on a loaded rule', () => {
    expect(canManageQuarantineExemption(makeUser(ADMIN), loadedRule)).toBe(true);
  });

  it('denies while userInfo is still loading', () => {
    // whoami is in flight: render nothing instead of flashing an admin control
    expect(canManageQuarantineExemption(null, loadedRule)).toBe(false);
  });

  it('denies non-admin roles, including the object-serialized developer', () => {
    expect(canManageQuarantineExemption(makeUser(USER), loadedRule)).toBe(false);
    // the Developer role serializes as an object, so the check must go through isAdmin
    expect(canManageQuarantineExemption(makeUser(DEV), loadedRule)).toBe(false);
  });

  it('denies the blank placeholder even for an admin', () => {
    // a missing or mismatched route id leaves BlankYaraRule in place; exempting it would
    // POST /entities/rules//quarantine-exemption with an empty id
    expect(canManageQuarantineExemption(makeUser(ADMIN), structuredClone(BlankYaraRule))).toBe(false);
  });
});
