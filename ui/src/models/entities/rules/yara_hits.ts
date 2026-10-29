// project imports
import { CreateEntity, Entities, Entity } from '../entities';

/**
 * A single yara string that matched within a fired rule (capped by the API).
 */
export type MatchedString = {
  // the identifier of the string that matched (e.g. `$marker`)
  id: string;
  // the total number of times this string matched
  count: number;
  // the first offsets this string matched at (capped)
  first_offsets: number[];
};

/**
 * A single rule that matched within a yara source document (capped by the API).
 */
export type MatchedRule = {
  // the identifier of the rule that matched within its source document
  rule_identifier: string;
  // the strings that matched for this rule (capped)
  strings: MatchedString[];
};

/**
 * The machine-written metadata of a yara rule hit against a sample.
 *
 * Hits are written by the scan path and hunts, never through the UI; the hit records the
 * exact rule text that fired (`rule_hash`), the ruleset build it fired in, and the engine
 * that evaluated it so a hit stays interpretable after the rule is edited.
 */
export type YaraRuleHitMetaFields = {
  // the entity id of the yara rule that produced this hit
  rule_id: string;
  // the sha256 of the sample this hit is for
  sha256: string;
  // the name of the yara rule entity at scan time
  rule_name: string;
  // the content hash of the rule source that fired
  rule_hash: string;
  // the hash of the compiled ruleset build this hit came from
  ruleset_hash: string;
  // the yara engine version that evaluated this rule
  engine_version: string;
  // when this hit was produced
  scanned_at: string;
  // the rules within the source document that matched (capped)
  matched: MatchedRule[];
};

export type YaraRuleHitMeta = {
  YaraRuleHit: YaraRuleHitMetaFields;
};

// hits are machine-written and never created through the UI; the create twin exists only to
// keep the kind-indexed create maps total (the Folder/WindowsProcess pattern)
export type YaraRuleHitCreateMeta = {
  YaraRuleHit: YaraRuleHitMetaFields;
};

export type YaraRuleHit = Entity<Entities.YaraRuleHit>;

export type CreateYaraRuleHit = CreateEntity<Entities.YaraRuleHit>;

export const BlankYaraRuleHit: YaraRuleHit = {
  id: '',
  name: '',
  groups: [],
  description: null,
  kind: Entities.YaraRuleHit,
  metadata: {
    YaraRuleHit: {
      rule_id: '',
      sha256: '',
      rule_name: '',
      rule_hash: '',
      ruleset_hash: '',
      engine_version: '',
      scanned_at: '',
      matched: [],
    },
  },
  tags: {},
  submitter: '',
  created: '',
  image: null,
};
