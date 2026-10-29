// project imports
import { KindDepth } from '../events';
import { CreateEntity, Entities, Entity } from './entities';

/**
 * The rule engine a hunt runs.
 *
 * The values are the exact lowercase wire strings the API's serde expects.
 */
export enum HuntEngine {
  Yara = 'yara',
}

/**
 * The per-engine info the UI needs to stay engine-aware.
 */
export type HuntEngineInfo = {
  // the display label for this engine
  label: string;
  // the entity kind of this engine's rules
  ruleKind: Entities;
  // the entity kind of this engine's findings
  hitKind: Entities;
};

/**
 * The engine registry: the UI mirror of the API's `HuntEngine::rule_kind()`.
 *
 * The single engine-awareness point — the create page's engine dropdown and rule picker
 * and the details page's "View hits" link all read this map, so a future sigma engine is
 * one `HuntEngine` member plus one entry here.
 */
export const HUNT_ENGINES: Record<HuntEngine, HuntEngineInfo> = {
  [HuntEngine.Yara]: { label: 'Yara', ruleKind: Entities.YaraRule, hitKind: Entities.YaraRuleHit },
};

/**
 * The lifecycle of a hunt; values are the exact wire strings and `HuntStatus` tag values.
 */
export enum HuntStatus {
  Created = 'Created',
  Running = 'Running',
  Completed = 'Completed',
  Cancelled = 'Cancelled',
  Failed = 'Failed',
}

/**
 * Whether a hunt status still counts against the active quotas (Created or Running).
 *
 * @param status - The hunt status to check.
 * @returns True while the hunt is Created or Running.
 */
export function isActiveHuntStatus(status: HuntStatus): boolean {
  return status === HuntStatus.Created || status === HuntStatus.Running;
}

// the intrinsic tag keys hunts and their findings are browsed by
export const HUNT_STATUS_TAG = 'HuntStatus';
export const HUNT_ACTIVE_TAG = 'HuntActive';
export const HUNT_ENGINE_TAG = 'HuntEngine';
export const HUNT_ID_TAG = 'HuntId';

/**
 * How a hunt chose its rules, kept for display; the frozen snapshot is what scans.
 *
 * Externally tagged like the API enum: `All` is a bare string on the wire.
 */
export type HuntSelector = { Rules: string[] } | { Tag: { key: string; value: string } } | 'All' | { RuleSet: string };

/**
 * One lane: a contiguous time shard of the corpus, walked newest to oldest.
 */
export type HuntLane = {
  // the newest timestamp this lane covers
  start: string;
  // the upload timestamp of the oldest line this lane has checkpointed
  current: string;
  // the oldest timestamp this lane covers
  end: string;
  // the checkpoint sequence number this lane expects next
  seq: number;
  // the server-side list cursor this lane is walking, while one is alive
  cursor: string | null;
  // whether this lane has exhausted its time range
  exhausted: boolean;
};

/**
 * Scan counters persisted at each checkpoint.
 *
 * Counters are operational telemetry: a chunk replayed after a lost checkpoint re-counts
 * its work, so totals can drift. The `HuntId` tag browse is the exact record of findings.
 */
export type HuntCounters = {
  listed: number;
  scanned: number;
  matched: number;
  written: number;
  suppressed: number;
  skipped: number;
  missing: number;
  errors: number;
  // a bounded sample of per-item errors
  error_samples: string[];
};

export const BlankHuntCounters: HuntCounters = {
  listed: 0,
  scanned: 0,
  matched: 0,
  written: 0,
  suppressed: 0,
  skipped: 0,
  missing: 0,
  errors: 0,
  error_samples: [],
};

/**
 * The shared corpus bounds: the file/repo list params shape.
 */
export type HuntCorpusBounds = {
  // the groups the corpus lists from; a subset of the hunt's groups
  groups: string[];
  // the newest bound; clamped to the hunt's creation instant
  start: string;
  // the oldest bound; defaults to the corpus kind's configured earliest
  end: string;
  // tag filters; strict AND across every pair, like the file list
  tags: Record<string, string[]>;
  // whether tag matching is case-insensitive
  tags_case_insensitive: boolean;
};

/**
 * The corpus a hunt walks: a kind picking the listing, plus shared bounds.
 */
export type HuntCorpus = { Files: HuntCorpusBounds } | { Repos: HuntCorpusBounds };

/**
 * The machine-written metadata of a user-created hunt.
 *
 * Hunts are created through the dedicated JSON route and advanced by checkpoints; the
 * generic entity update path never touches this metadata, so the UI renders it read-only.
 */
export type HuntMetaFields = {
  // the rule engine this hunt runs
  engine: HuntEngine;
  // the selector as submitted, kept for display
  selector: HuntSelector;
  // the frozen snapshot of rule entity ids resolved at creation
  rules: string[];
  // how many rules the engine's snapshot filter excluded (yara: Noisy/Slow)
  excluded_quarantined: number;
  // how many RuleSet members of another engine's kind were excluded
  excluded_foreign: number;
  // the corpus this hunt walks
  corpus: HuntCorpus;
  // where this hunt is in its lifecycle
  status: HuntStatus;
  // per-lane progress through the corpus
  lanes: HuntLane[];
  // scan counters, persisted at each checkpoint (serde-defaulted on older hunts)
  counters?: HuntCounters;
  // why this hunt was marked Failed if the system stopped it
  failure?: string | null;
};

export type HuntMeta = {
  Hunt: HuntMetaFields;
};

// hunts are created through the dedicated JSON route, never the generic multipart create;
// the create twin exists only to keep the kind-indexed create maps total
export type HuntCreateMeta = {
  Hunt: HuntMetaFields;
};

/**
 * One frozen rule line from `GET /entities/hunt/{id}/rules`.
 *
 * The frozen snapshot is immutable, but each line hydrates its display fields from the
 * CURRENT rule entity: `name`/`kind`/`created` are absent when the rule was deleted after
 * the freeze or is invisible to the caller, so those lines render id-only and linkless.
 * `status` carries the rule's current `YaraRuleStatus` tag values (not the values at
 * freeze time) and `quarantine_exempt` its current exemption flag; serde omits the
 * empty/default forms on the wire, so the thorpi wrapper normalizes both before lines
 * reach any component.
 */
export interface HuntRuleLine {
  // the frozen rule entity id
  id: string;
  // the rule's name, absent when it was deleted after the freeze or is hidden
  name?: string;
  // the rule's entity kind, absent when it was deleted after the freeze or is hidden;
  // a newer server can serve kinds outside this build's enum, which render linkless
  kind?: Entities;
  // when the rule was created, absent when it was deleted after the freeze or is hidden
  created?: string;
  // the rule's current quarantine status tag values (e.g. Noisy/Slow/Broken)
  status: string[];
  // whether the rule is currently exempt from quarantine
  quarantine_exempt: boolean;
}

export type Hunt = Entity<Entities.Hunt>;

export type CreateHuntEntity = CreateEntity<Entities.Hunt>;

export const BlankHunt: Hunt = {
  id: '',
  name: '',
  groups: [],
  description: null,
  kind: Entities.Hunt,
  metadata: {
    Hunt: {
      engine: HuntEngine.Yara,
      selector: 'All',
      rules: [],
      excluded_quarantined: 0,
      excluded_foreign: 0,
      corpus: { Files: { groups: [], start: '', end: '', tags: {}, tags_case_insensitive: false } },
      status: HuntStatus.Created,
      lanes: [],
      counters: BlankHuntCounters,
      failure: null,
    },
  },
  tags: {},
  submitter: '',
  created: '',
  image: null,
};

/**
 * The all-optional request mirror of the corpus bounds; omitted bounds use server defaults.
 */
export type HuntCorpusBoundsRequest = {
  groups?: string[];
  start?: string;
  end?: string;
  tags?: Record<string, string[]>;
  tags_case_insensitive?: boolean;
};

/**
 * The request mirror of the corpus; Repos is rejected by the API until an engine scans repos.
 */
export type HuntCorpusRequest = { Files: HuntCorpusBoundsRequest } | { Repos: HuntCorpusBoundsRequest };

/**
 * A request to create a hunt via `POST /entities/hunt/`.
 */
export type HuntCreateRequest = {
  // the name for the hunt entity
  name: string;
  // the groups this hunt, and everything it writes, is bounded to
  groups: string[];
  // the rule engine this hunt runs
  engine: HuntEngine;
  // how to choose the rules this hunt freezes
  selector: HuntSelector;
  // the corpus; omitted bounds default to everything up to creation
  corpus?: HuntCorpusRequest;
  // a description for the hunt entity
  description?: string;
};

/**
 * One hunt on the admin status page: a bounded projection of a hunt entity.
 *
 * The frozen rule snapshot is reduced to a count while the small, bounded lanes
 * and counters ride whole so the page can reuse the lane progress math.
 */
export type HuntStatusLine = {
  // the unique id of this hunt
  id: string;
  // the name of this hunt
  name: string;
  // the user that created this hunt
  submitter: string;
  // the groups this hunt is visible to
  groups: string[];
  // the engine this hunt scans with
  engine: HuntEngine;
  // the current status of this hunt
  status: HuntStatus;
  // when this hunt was created (RFC3339)
  created: string;
  // how many rules the frozen snapshot holds (never the ids themselves)
  frozen_rules: number;
  // the time lanes this hunt walks and their progress
  lanes: HuntLane[];
  // the cumulative counters for this hunt
  counters: HuntCounters;
  // why this hunt failed if it did; serde-skipped when absent
  failure?: string;
};

/**
 * One user's hunt usage in the admin census.
 */
export type UserHuntCensus = {
  // the username this census row covers
  username: string;
  // how many Created or Running hunts this user has right now
  active: number;
  // the per-user active quota (thorium.hunts.max_active_per_user)
  quota: number;
  // how many hunts this user created inside the recent window (saturating)
  recent_created: number;
};

/**
 * The configured quotas and current global usage.
 */
export type HuntQuotaView = {
  // the per-user active hunt quota
  max_active_per_user: number;
  // the global active hunt quota
  max_active_global: number;
  // how many hunts are active globally right now
  active_global: number;
};

/**
 * The admin hunt census (`GET /entities/hunt/status/`); admin only.
 */
export type HuntSystemStatus = {
  // hunts currently running
  ongoing: HuntStatusLine[];
  // hunts created but not yet running (queued, not scheduled)
  upcoming: HuntStatusLine[];
  // recently terminal hunts inside the window, newest first
  recent: HuntStatusLine[];
  // per-user usage against quotas
  users: UserHuntCensus[];
  // queue depths for the hunt chunk kinds (the paced backlog, keyed by EventType)
  chunk_backlog: Record<string, KindDepth>;
  // the quotas and global usage
  quotas: HuntQuotaView;
};
