# Design

---

Seven decisions carry this design. Everything else — models, events, worker wiring,
caches — follows from them; each gets a subsystem page below.

## Core Decisions

### 1. Rules are entities

**Decision: add `EntityMetadata::YaraRule` beside sigma; no new Scylla tables anywhere.**

The `YaraRule` metadata model lands in a new `api/src/models/entities/rules/yara.rs`,
exported next to `SigmaRule` (`api/src/models/entities/rules/sigma.rs:95-105`), and
`EntityKinds::YaraRule` falls out of the existing discriminant derive. The entities table
already stores one row per group with a TEXT `kind` and JSON `kind_data`
(`api/src/models/schema/scylla/entities.rs`), so rules — and hits
(decision 4) — inherit group scoping, authz, census, intrinsic tags, listing cursors, and
thoradm backup with zero schema work. Entities, associations, and tags are reused end to
end (see [YaraRule Entity](./rule_entity.md)).

### 2. One global compiled ruleset, namespace-per-rule

**Decision: compile every rule entity into one global ruleset, one yara-x namespace per
rule named by the entity UUID; enforce visibility at hit-write time, never at scan time.**

Byte scanning is the expensive operation, and yara-x shares its atom pass across all
rules in a single scan — so every sample is scanned exactly once with all rules, where
per-group rulesets would re-scan the same bytes per group *and* still need per-hit group
math for multi-group samples. The UUID namespace makes hit attribution exact by
construction. The price is per-rule containment: compile failures, noisy rules, and
pathologically slow rules are quarantined individually (`YaraRuleStatus=Broken|Noisy|Slow`
tags plus a submitter notification) rather than trusted — the sigma context used to
compile with a bare `.unwrap()`, where one malformed stored rule panicked the whole
daemon (log-and-skip since M1, `event-handler/src/libs/workers/hunts/sigma.rs:129-143`)
(see [Ruleset Compilation and Caching](./ruleset.md)).

### 3. Write-time group intersection

**Decision: `write_groups = rule.groups ∩ event.groups ∩ allowable_groups`, computed per
event by the service account — no uploader masquerade.**

Sigma scans as the uploader through per-user masqueraded clients
(`event-handler/src/libs/workers/hunts/sigma.rs:959-980`) and writes into the rule's
groups; entity create authorizes the writer in **all** requested groups at once
(`api/src/models/backends/entities.rs:173-182`), so an uploader outside a rule's group 401s
and the cross-group hit drops with only a log line. Partitioned hit writes are a *system*
invariant, not a user action: the `YaraHitWriter` is the single audited code path that
computes write groups. The `allowable_groups` term intersects away groups whose per-group
switches disable entities, associations, or tags — those checks bind even admin writers,
and suppressed groups are counted in stats, never silently merged into one all-groups
call that fails whole. Hit groups accumulate by merge as new submissions widen a sample's
visibility, and shrink again on revocation via `YaraScanTrigger::GroupsRemoved` (see
[Visibility and Partitioning](./visibility.md)).

### 4. First-class, deterministic hits

**Decision: a `YaraRuleHit` entity whose identity derives from `(rule_id, sha256)`, wired
into the association graph and the tag system with propagated trigger depth.**

Deterministic identity makes hit writes commutative: retries, concurrent scanner
replicas, and rescans after a rule edit all converge on the same entity and merge groups,
instead of trusting a read-then-write association-tree walk to prevent duplicates. Each
hit gets `AssociationKind::CreatedBy` from its rule and a new
`AssociationKind::YaraRuleHit` to the sample — both additive variants in the TEXT-backed
kind enum (`api/src/models/associations.rs:46-99`) — plus `YaraRuleHits=<rule entity
name>` tags on the sample in each write group. Sample tags emit `NewTags` events
(`api/src/models/backends/db/tags.rs:103-107`), so the writer stamps them with
`event.depth + 1`: a defaulted depth of zero would reset trigger lineage and let any
trigger-plus-tool pair whose output re-matches a rule loop forever. Rules may also carry
optional sigma-style auto-flag `actions` (`YaraActionToTake::Flag(YaraAutoFlag)`) — see
[Hits and Attribution](./hits.md).

### 5. Event-driven, in the event-handler first

**Decision: a new `EventType::YaraScannableSample` with its own queue, consumed by a new
`EventWorkerKinds::YaraScanner`; scale-out and hunts are milestones, not
prerequisites.**

Scan events are emitted beside the existing `NewSample` event at sample create
(`api/src/models/backends/db/files.rs:287-300`) and on submission-update group changes —
a path that emitted no event at all before M1 (`api/src/models/backends/db/files.rs:900-967`).
The worker slots into the existing kind enum
(`event-handler/src/libs/workers.rs:113-121`) and reuses the pop/retry/broadcast
machinery, which is the smallest diff and full sigma parity. Scale-out keeps the same
venue — M3 makes the event-handler itself horizontally scalable rather than promoting
the scanner into a dedicated deployment — and hunts over pre-existing samples (M4) ride
the same event machinery through their own queue and worker (see
[Scan Triggering](./triggering.md), [Scan Execution](./execution.md), and
[Hunts and Rule Sets](./hunts.md)).

### 6. Freshness is first-class

**Decision: per-component cache-status flags so new rules go live in tens of seconds.**

Before M1 the event-handler's refresh loop rebuilt only the trigger cache and entity
CRUD set no cache flag, so sigma rules were frozen at process start. The single status
boolean splits into per-component
fields, every `YaraRule` mutation flips the `yara_rules` flag, and the controller rebuilds
and broadcasts the compiled context — bounding rule-upload-to-live staleness at poll plus
debounce plus measured compile time (see
[Cache Invalidation and Freshness](./cache.md)).

**Reversed: the `YaraScanned` coverage ledger.** M2 also shipped a per-sample
`YaraScanned=<ruleset_hash>` tag on every completed scan so "did my rule run on this
sample?" would have an exact answer. It was removed after M4: every scan paid a tag write
plus a `NewTags` event, every ruleset change churned every scanned sample's tag row, and
the consumer it anticipated — the backscan — was replaced by hunts, so nothing ever read
it. Deliberate skips keep their `YaraScanSkipped` markers, matches keep their build
provenance (`ruleset_hash` on every hit), and an unmatched, unskipped sample has no
per-sample scan record (see [Scan Triggering](./triggering.md)).

### 7. History is hunted on demand, never auto-rescanned

**Decision: no automatic rescan of history on rule create/update — users create hunts:
bounded, paced passes of a frozen rule snapshot over the pre-existing corpus, with every
finding attributed to the hunt. Hunts are a generic chassis with per-engine plugs; M4
ships the chassis plus the yara engine.**

Rescanning a corpus re-reads every byte of it, so it is a capacity decision a user makes
explicitly — bounded by per-user and global quotas and the system's first pacing
configuration — never a side effect of editing a rule. A hunt names its rule engine,
freezes its rule selection (explicit ids, a tag, everything visible, or a `RuleSet`
entity) to a snapshot at creation, walks a corpus that defaults to everything visible up
to that instant, and converges onto the engine's own deterministic findings — decorated
with an engine-blind hunt association and tag so "what did my hunt find?" is one query.
For the yara engine those findings are the same deterministic hits the scan path writes;
engine-upgrade refresh and rule-group-widening repair become documented hunt runbooks
(see [Hunts and Rule Sets](./hunts.md), and
[Generic Hunt Trade-offs](./hunt_tradeoffs.md) for why generic and what it costs).

## Subsystem Pages

| Page | Covers |
| --- | --- |
| [YaraRule Entity](./rule_entity.md) | `YaraRule` model, compile validation, intrinsic tags, forms/client/UI surface |
| [Engine Choice](./engine.md) | Why `yara-x` over libyara FFI and the CLI; compatibility and upgrade policy |
| [Ruleset Compilation and Caching](./ruleset.md) | The global `YaraRuleContext`, quarantine, `ruleset_hash`, serialized cache |
| [Scan Triggering](./triggering.md) | `YaraScannableSample` emission sites, `YaraScanTrigger` variants, coalescing |
| [Scan Execution](./execution.md) | Venue choice, ownership-scoped reclaim, download/uncart, timeouts and size caps, scan concurrency |
| [Visibility and Partitioning](./visibility.md) | The intersection formula, the no-leak argument, group merge and revocation |
| [Hits and Attribution](./hits.md) | `YaraRuleHit` identity and metadata, associations, tags, delete semantics |
| [Hunts and Rule Sets](./hunts.md) | `Hunt` and `RuleSet` entities, frozen rule snapshots, chunk-chained execution, checkpointing, finding decoration, the engine plug points |
| [Generic Hunt Trade-offs](./hunt_tradeoffs.md) | Why hunts are a generic chassis with per-engine plugs, what stays engine-specific, the sigma engine's prerequisites, costs accepted |
| [Scaling and Performance](./performance.md) | Cost model, write amplification, noisy/slow-rule control, queue health |
| [Cache Invalidation and Freshness](./cache.md) | Per-component cache flags and the staleness bound |
