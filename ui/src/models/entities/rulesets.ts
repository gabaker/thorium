// project imports
import { CreateEntity, Entities, Entity } from './entities';

/**
 * A member rule hydrated into a rule set at read time.
 *
 * The API hydrates `RuleSetMember` associations into full rule entities; the UI only
 * reads the identifying fields, so this structural type keeps extra wire fields legal
 * while `kind` (SigmaRule or YaraRule — members may be of any rule engine) drives the
 * member's details link.
 */
export type RuleSetMember = {
  id: string;
  name: string;
  kind: Entities;
  // when the member rule was created, hydrated by the API at read time; client-built
  // pending-fallback members (see resolvePendingMembers) lack it, so a bounded time
  // filter treats them as out of range until a refetch hydrates them
  created?: string;
};

export type RuleSetMetaFields = {
  // hydrated from RuleSetMember associations at read time
  rules?: RuleSetMember[];
};

export type RuleSetCreateMetaFields = {
  // member rule entity ids to associate at create time
  rules?: string[];
};

export type RuleSetMeta = {
  RuleSet: RuleSetMetaFields;
};

export type RuleSetCreateMeta = {
  RuleSet: RuleSetCreateMetaFields;
};

// `YaraRuleset` (the yara rule's metadata fields) is already taken, so the entity type
// carries the `Entity` suffix to stay unambiguous
export type RuleSetEntity = Entity<Entities.RuleSet>;

export type CreateRuleSetEntity = CreateEntity<Entities.RuleSet>;

export const BlankRuleSet: RuleSetEntity = {
  id: '',
  name: '',
  groups: [],
  description: null,
  kind: Entities.RuleSet,
  metadata: {
    RuleSet: {
      rules: [],
    },
  },
  tags: {},
  submitter: '',
  created: '',
  image: null,
};

export const BlankCreateRuleSet: CreateRuleSetEntity = {
  name: '',
  groups: [],
  tags: {},
  description: null,
  kind: Entities.RuleSet,
  metadata: {
    RuleSet: {
      rules: [],
    },
  },
};
