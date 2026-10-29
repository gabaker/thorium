import { CreateEntity, Entities, Entity } from '../entities';

export enum YaraRuleAppliesTo {
  /// Apply this rule to raw sample bytes
  Samples = 'Samples',
}

/// Automatically promote this yara rule hit to a flag
export type YaraAutoFlag = {
  /// The interesting, odd, or suspicious characteristic
  content?: string;
  /// The reason for this Flag
  reasoning: string;
};

export enum YaraRuleActions {
  Flag = 'Flag',
}

/// The entity tag key the event handler writes quarantine status under
/// (values like Noisy/Slow/Broken); mirrors the API's `YaraRule::STATUS_TAG`
export const YARA_RULE_STATUS_TAG = 'YaraRuleStatus';

/// The status values a quarantine clear may remove; mirrors the set the API's
/// exemption route clears (`YaraRule::{STATUS_NOISY, STATUS_SLOW}`). `Broken`
/// is deliberately not clearable: a broken rule cannot scan, and deleting its
/// tag would just get it re-tagged (and its submitter re-notified) by the next
/// scanner restart — fixing the rule source is what clears Broken.
export const CLEARABLE_QUARANTINE_STATUSES: string[] = ['Noisy', 'Slow'];

/// Map each action to its payload type
export type YaraActionActionType = {
  [YaraRuleActions.Flag]: YaraAutoFlag;
};

/// A single action variant
export type YaraActionToTake = {
  [K in keyof YaraActionActionType]: {
    [P in K]: YaraActionActionType[K];
  };
}[keyof YaraActionActionType];

/// A YARA rule that can be applied to raw sample bytes
export type YaraRuleset = {
  /// The original unparsed rule source
  rule: string;
  /// What types of data this yara rule applies too
  applies_to: YaraRuleAppliesTo[];
  /// The score to apply when this rule hits
  score: number;
  /// The actions to take when this yara rule hits
  actions: YaraActionToTake[];
  /// Whether this rule is exempt from noisy/slow quarantine; settable only by
  /// admins through the dedicated exemption route (serde-defaulted on older rules)
  quarantine_exempt?: boolean;
};

export type YaraRuleMetaFields = YaraRuleset;

export type YaraRuleMeta = {
  YaraRule: YaraRuleset;
};

export type YaraRuleCreateMeta = {
  YaraRule: YaraRuleset;
};

export type YaraRule = Entity<Entities.YaraRule>;

export type CreateYaraRule = CreateEntity<Entities.YaraRule>;

export const BlankYaraRule: YaraRule = {
  id: '',
  name: '',
  groups: [],
  description: null,
  kind: Entities.YaraRule,
  metadata: {
    YaraRule: {
      rule: '',
      score: 0,
      applies_to: [],
      actions: [],
    },
  },
  tags: {},
  submitter: '',
  created: '',
  image: null,
};

export const BlankCreateYaraRule: CreateYaraRule = {
  name: '',
  groups: [],
  tags: {},
  description: null,
  kind: Entities.YaraRule,
  metadata: {
    YaraRule: {
      rule: '',
      score: 0,
      applies_to: [],
      actions: [],
    },
  },
};
