import { describe, it, expect } from 'vitest';

// project imports
import { Entities } from './entities';
import {
  HUNT_ACTIVE_TAG,
  HUNT_ENGINE_TAG,
  HUNT_ENGINES,
  HUNT_ID_TAG,
  HUNT_STATUS_TAG,
  HuntEngine,
  HuntRuleLine,
  HuntStatus,
  isActiveHuntStatus,
} from './hunts';

describe('HuntEngine / HUNT_ENGINES', () => {
  it('pins the lowercase wire value for yara', () => {
    // the API's serde rename_all = "lowercase" — "Yara" is a 400
    expect(HuntEngine.Yara).toBe('yara');
  });

  it('maps every engine to real rule and hit kinds', () => {
    for (const engine of Object.values(HuntEngine)) {
      const info = HUNT_ENGINES[engine];
      expect(info).toBeTruthy();
      expect(info.label).toBeTruthy();
      expect(Object.values(Entities)).toContain(info.ruleKind);
      expect(Object.values(Entities)).toContain(info.hitKind);
    }
  });

  it('maps yara to the yara rule and hit kinds', () => {
    expect(HUNT_ENGINES[HuntEngine.Yara].ruleKind).toBe(Entities.YaraRule);
    expect(HUNT_ENGINES[HuntEngine.Yara].hitKind).toBe(Entities.YaraRuleHit);
  });
});

describe('HuntStatus', () => {
  it('pins the exact wire strings', () => {
    expect(Object.values(HuntStatus)).toEqual(['Created', 'Running', 'Completed', 'Cancelled', 'Failed']);
  });

  it('counts exactly Created and Running as active', () => {
    expect(isActiveHuntStatus(HuntStatus.Created)).toBe(true);
    expect(isActiveHuntStatus(HuntStatus.Running)).toBe(true);
    expect(isActiveHuntStatus(HuntStatus.Completed)).toBe(false);
    expect(isActiveHuntStatus(HuntStatus.Cancelled)).toBe(false);
    expect(isActiveHuntStatus(HuntStatus.Failed)).toBe(false);
  });
});

describe('hunt tag keys', () => {
  it('pins the intrinsic tag keys the API stamps', () => {
    expect(HUNT_STATUS_TAG).toBe('HuntStatus');
    expect(HUNT_ACTIVE_TAG).toBe('HuntActive');
    expect(HUNT_ENGINE_TAG).toBe('HuntEngine');
    expect(HUNT_ID_TAG).toBe('HuntId');
  });
});

describe('HuntRuleLine', () => {
  it('pins the exact wire field names of a frozen rule line', () => {
    // a fully hydrated line as GET /entities/hunt/{id}/rules serializes it (status and
    // quarantine_exempt are serde-skipped/defaulted on the wire; thorpi normalizes them)
    const line: HuntRuleLine = {
      id: '0a658ba7-78a9-4a2e-8df3-7fbf18960286',
      name: 'upx',
      kind: Entities.YaraRule,
      created: '2026-10-17T00:00:00Z',
      status: ['Noisy'],
      quarantine_exempt: true,
    };
    expect(Object.keys(line)).toEqual(['id', 'name', 'kind', 'created', 'status', 'quarantine_exempt']);
  });

  it('keeps the hydrated display fields optional for deleted or hidden rules', () => {
    // a rule deleted after the freeze (or invisible to the caller) hydrates nothing
    const line: HuntRuleLine = { id: '0a658ba7-78a9-4a2e-8df3-7fbf18960286', status: [], quarantine_exempt: false };
    expect(line.name).toBeUndefined();
    expect(line.kind).toBeUndefined();
    expect(line.created).toBeUndefined();
  });
});
