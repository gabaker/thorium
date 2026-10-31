# Rule Scanning
---
Thorium scans every sample it ingests against user-written YARA rules, scans the structured entities tools attach to
results against Sigma rules, and lets users hunt over historical data. This page explains the workers and queues, the
global compiled ruleset, how hits are written and partitioned, and how hunts pace themselves. There is no separate
scanner deployment — everything runs inside the API and the [event handler](./event-handler.md), on Redis, Scylla, and S3.

> This page is internals documentation for Thorium admins. For day-to-day usage see [YARA Rules](../users/yara_rules.md),
> [Sigma Rules](../users/sigma_rules.md), [Rule Sets](../users/rule_sets.md), and [Hunts](../users/hunts.md); for the
> config knobs, monitoring, and remediation runbook see [Operate Rule Scanning and Hunts](../admins/rule_scanning_ops.md).

## Components and Queues
---
The event handler is a single process containing a controller, four workers, and a heartbeat task. The controller
runs a 5 second control loop that polls cache staleness and handles SIGTERM; a failed worker or heartbeat task is
fatal — the process drains its in-flight events and exits so Kubernetes restarts the pod. Each worker
consumes exactly one Redis event queue:

| Worker | Event kind consumed | Work |
|--------|---------------------|------|
| `ReactionTriggers` | `ReactionTrigger` | Evaluates pipeline tag triggers and spawns reactions |
| `SigmaScanner` | `SigmaScannableResults` | Scans scannable entities attached to new tool results |
| `YaraScanner` | `YaraScannableSample` | Scans sample bytes against the global compiled ruleset |
| `YaraHunter` | `YaraHuntChunk` | Advances one lane of one hunt by one chunk |

The heartbeat is a dedicated task (the control loop can block for multi-second YARA compiles): every
`thorium.events.heartbeat_secs` (default 10 s) it upserts this process's registry entry, invokes the server-side
reaper, and logs per-kind queue depths. If the heartbeat task ever dies, the controller exits the whole process so
Kubernetes restarts the pod. Each process identifies itself as `{hostname}-{8 random hex chars}` (the pod name under
Kubernetes) and never reuses an old identity — anything a dead identity left in flight returns via drain or reaper.

## The Global Compiled Ruleset
---
Live YARA scanning uses one compiled ruleset holding every stored `YaraRule` across all groups. The build is
quarantine aware: rules tagged `YaraRuleStatus=Noisy` or `Slow` are skipped up front, and each remaining source is
solo-compiled first (classify) so a broken rule is quarantined instead of failing the combined build — one bad
stored rule can never crash the daemon. The surviving sources are hashed into a `ruleset_hash` (a SHA256 over the
sorted rule id and rule hash pairs) *before* the expensive combined compile, so the hash can key a cache:
`GET /api/events/yara/ruleset/{engine_version}/{ruleset_hash}/` serves serialized rulesets from the S3 bucket
`thorium.yara.cache_bucket` (default `thorium-yara-rulesets`) at object key `{engine_version}/{ruleset_hash}`
(e.g. `yara-x-1.21.0/<sha256>`). A cache hit deserializes instead of compiling; a miss compiles locally and uploads
for peers, pruning same-engine-version siblings so the bucket holds roughly one live object per engine version.
Every cache failure is non-fatal: the event handler warns and compiles locally.

Staleness is tracked with monotonic generation counters in the Redis hash `{ns}:event-handler:cache`: every rule
create, update, or delete — and every entity tag write on a rule, like a quarantine tag — bumps a generation, and
the controller compares generations rather than clearing flags, so replicas can all poll the same hash. Rebuilds are
debounced by `max(30 s, last compile duration)` so rule churn cannot stack overlapping compiles, and a full rebuild
every 10 minutes backstops lost generation bumps; workers scan the previous ruleset until the swap arrives.

> Net effect: one rule change costs roughly one compile cluster-wide, and restarts deserialize instead of recompiling.

## The Live Scan Path
---
Uploading a sample — or adding or removing one of its submission groups — emits a `YaraScannableSample` event. The
`YaraScanner` worker pops up to 1000 events and coalesces them by SHA256: added groups union into one scan's write set,
removals only mark the sample for a shrink pass (revoked groups never enter the write set), and the max depth is kept.

Sample bytes are downloaded and uncarted entirely in memory — the scanner never writes sample bytes to disk.
Downloads are admitted under a two-class byte budget: each sample starts with a small-class grant
(`thorium.yara.small_class_size`, default 16 MiB) and upgrades to a bounded large-class grant of up to
`thorium.yara.max_scan_size` (default 1 GiB), all inside the per-replica `thorium.yara.scan_buffer_budget` total.
Scans run on blocking threads with a per-scan timeout (`thorium.yara.scan_timeout_secs`, default 60 s) and end in
one of four outcomes:

| Outcome | Meaning | Event |
|---------|---------|-------|
| `Scanned` | The scan completed and any hits were written. | Cleared |
| `Oversize` | The sample exceeds `max_scan_size`; a `YaraScanSkipped=oversize:<CAP>` file tag records the deliberate skip. | Cleared |
| `Missing` | The sample was deleted (404); treated as success-with-skip. | Cleared |
| `Failed` | A download or scan error occurred. | Stays in flight and retries after 3 minutes |

A completed scan writes nothing beyond its hits.
Pure-revocation events skip the download and shrink instead: the worker re-lists the sample's `YaraRuleHit`
associations, recomputes each hit's groups against the sample's current groups, and removes or deletes accordingly.

## Hit Writes and Write-Time Visibility
---
Scanner replicas never write hits to the databases themselves. Every hit is one call to the admin-gated route
`POST /api/entities/yara/hit/` carrying only scan-time facts: rule id, SHA256, rule hash, ruleset hash, engine
version, scan time, the event's groups and depth, the bounded matched strings, and optionally a hunt id. The API
recomputes visibility from fresh database state at write time as
`write_groups = rule.groups ∩ event.groups ∩ sample.groups ∩ allowable_groups`, where `allowable_groups` drops
groups whose kill switches disable entities, associations, or tags — those bind even the admin service account. The
worker's cached intersection is only a pre-filter: it can under-write (the server reports it back), never over-write.

Hit identity is deterministic over `(rule_id, sha256)`, so rescans, retries, and hunts all converge onto the same
`YaraRuleHit` entity instead of duplicating it; a newer scan refreshes the stored scan facts and groups merge. Each
write also creates the `CreatedBy` (rule → hit) and `YaraRuleHit` (hit → sample) associations, stamps the
`YaraRuleHits=<RULE_NAME>` sample tag at `event depth + 1` (see
[Interplay with Reaction Triggers](#interplay-with-reaction-triggers)), runs the rule's `Flag` auto-flag actions
from its fresh entity, and — for hunt scans — decorates the hit with a `HuntHit` association and a `HuntId` entity tag.

## Rule Quarantine
---
The scanner quarantines misbehaving rules by tagging them `YaraRuleStatus` and writing a notification on the rule
that records the reason for its submitter (readable via `GET /api/entities/notifications/{id}`):

| `YaraRuleStatus` | Trigger |
|------------------|---------|
| `Broken` | The rule failed to compile during classify. |
| `Noisy` | The rule matched more than 20% of the last 1000 scans (`thorium.yara.noisy_threshold` over `noisy_window`, tracked per replica). |
| `Slow` | The profiler attributed the rule in 3 scans that ran at least 5000 ms (`thorium.yara.slow_strikes` at `slow_rule_ms`). |

`Noisy` and `Slow` rules are skipped by live scans and excluded from new hunt snapshots, and the quarantine tag
write bumps the rule generation counter so every peer replica drops the rule at its next rebuild within seconds. The
exit is deleting the rule's `YaraRuleStatus` tag (any editor can, via `thorctl rules unquarantine`, the rule page's
**Clear quarantine** control, or the entity tag delete route) — the tag delete bumps the rule generation the same
way, so peers pick the rule back up at their next rebuild. Rule updates never touch quarantine tags, so a fixed rule
stays benched until its tag is removed.

Admins can mark a rule **exempt from quarantine** (`quarantine_exempt` on its metadata, settable only through the
admin-only `POST /api/entities/rules/{id}/quarantine-exemption` route): the scanner never suppresses, tags, or
notifies an exempt rule — exempt rules are filtered out of the noisy/slow candidate sets entirely — while other
rules keep being counted normally. Setting the exemption also deletes any current `Noisy`/`Slow` status values so
the rule re-enters rotation in the same action. Exemption never gates `Broken`: a rule that fails to compile drops
out at classify regardless. The context build's list-time skip and the hunt snapshot filter both honor the flag
too, so even a stale status tag that survived a quarantine race (a worker tagging concurrently with the admin's
exemption) can only linger cosmetically — it can never bench an exempt rule.

## Hunts: Lanes, Chunks, Checkpoints
---
A hunt is an on-demand pass of a frozen rule snapshot over historical samples. Creation resolves the selector under
the creator's authorization and freezes it into a rule id list at that instant: `Noisy`/`Slow` rules are excluded
and counted as `excluded_quarantined`, rule set members of another engine count as `excluded_foreign`, and `Broken`
rules are not excluded at create (their source simply drops out at compile); rules created later never join. The
corpus window is split into equal time-shard lanes (`thorium.hunts.lanes`, default 4), each walked newest to oldest,
with internal lane boundaries overlapping by one second because the corpus page pull is strict on both ends.
Creation seeds a per-lane sequence key in Redis and emits one `YaraHuntChunk` event per lane at depth 0.

The `YaraHunter` worker pops exactly one chunk per pop — a chunk is minutes of work, so the lease matches the work
unit and a SIGTERM replays at most one chunk. Per chunk it:

1. Loads the hunt — a terminal status, a stale sequence number, a missing lane, or a deleted hunt drops the chain
   (chains collapse, they never fork).
2. Loads the compiled rule subset through a per-worker LRU keyed by the frozen snapshot (identical snapshots share
   one build); subset builds only `GET` the ruleset cache, never upload — an upload would evict the live global object.
3. Lists one `thorium.hunts.chunk_size` slice (default 500 lines) from the lane's cursor, dedupes it by SHA256, and
   scans it under the hunt-dedicated `thorium.hunts.yara.scan_buffer_budget` byte budget.
4. Writes hits through the same hit route with the corpus groups as event groups, depth 0, and the hunt id.
5. Commits the chunk via `POST /api/entities/hunt/{id}/checkpoint`.

The checkpoint is a Redis compare-and-set against `{ns}:hunts:{hunt}:lane:{lane}:seq` (7 day TTL, refreshed per
commit): it compares the submitted sequence, increments it, and schedules the next chunk at
`now + thorium.hunts.inter_chunk_pacing_secs` (default 5 s) in the same atomic step; the future score rides the
pop's 3 second maturity floor, so pacing needs no extra queue machinery. A lost compare-and-set is a 409 and the
worker drops its stale chain; a lost Redis key is reseeded at the further along of the hunt entity's last persisted
sequence and the committing chunk's own sequence, so a lagging persisted view never rewinds a lane. The
entity's lanes and counters are just a persisted view of that Redis state, rewritten under a per-hunt mutex at
`{ns}:hunts:{hunt}:persist` (30 s TTL); cancels and deletes take the same mutex, and terminal transitions sweep
stale `HuntStatus`/`HuntActive` tag rows.

## Sigma Scanning (The Older Pattern)
---
Sigma scanning predates the YARA path and kept the original design. It is result-driven: a `SigmaScannableResults`
event fires when a tool result carrying scannable entities is created, and the `SigmaScanner` worker compiles every
stored `SigmaRule` at context build, logging and skipping compile failures (no quarantine, no notifications). Scans
run as the uploader through masqueraded per-user clients and a match performs the rule's actions: flag actions
create a `Flag` entity in the *rule's* groups, and `SpawnReaction` actions spawn a pipeline reaction with args
rendered from the matched entity by jq filters (see the [event handler](./event-handler.md) page). Spawned
reactions run as the uploader, so an uploader without access to a spawn action's target group or pipeline fails
authorization and that spawn is dropped (log and stats only). Flag entities and their associations are instead
created with the worker's base (admin) client, so a rule's flags land in the rule's groups regardless of the
uploader's group membership. Sigma has no `thorium.yml` config section and no hunt engine — the
fixes built for the YARA path have not yet been ported back.

## Scaling, Ownership, and the Reaper
---
Event handler replicas scale horizontally with no leader election. A pop is one atomic Redis operation: it pops
pending events, re-queues anything younger than 3 seconds (the Scylla consistency grace), claims the mature events
into the caller's per-owner in-flight map and queue (scored by lease time), and upserts the caller's registry
entry. Clears only touch the worker's own keys.

Every replica's heartbeat invokes the server-side reaper, which is concurrent-safe through per-entry atomic claims:
it drains owners whose last heartbeat is older than `thorium.events.worker_expiry_secs` (default 90 s) back to the
pending queues, reclaims leases of live-but-wedged owners past `thorium.events.in_flight_backstop_secs` (default
3600 s), and age-gates the legacy un-owned in-flight keys. Restored events keep their original scores, so order holds.
On SIGTERM the controller aborts the heartbeat, asks workers to stop between pages, waits a 20 second quiesce
grace, then drains its own in-flight entries back to pending — a rolling update replays at most mid-page work.
Duplicate processing is accepted and bounded: hit writes converge on their deterministic identity and duplicate
clears no-op, but duplicate *reactions* can be created — the same class of duplicate as any crash replay.

## Interplay with Reaction Triggers
---
Hit tags are ordinary file tags, so writing `YaraRuleHits=<RULE_NAME>` emits a `NewTags` event into the
`ReactionTrigger` queue, where pipeline tag triggers match it and spawn reactions at `event depth + 1`. The only
brake on these chains is `thorium.events.max_depth` (default 5): events at the limit are dropped. Hunts emit their
chunks at depth 0 deliberately — hunt hits land at depth 1 and *do* fire tag triggers, with chunk pacing and the
active-hunt quotas as the lid on the resulting burst.

## Where State Lives
---
**Redis** (`{ns}` is `thorium.namespace`; `{kind}` is one of the four event kinds above):

| Key | Type | Holds |
|-----|------|-------|
| `{ns}:event-handler:queue:{kind}` | ZSET | Pending events scored by timestamp (hunt chunks scored at now + pacing) |
| `{ns}:event-handler:in_flight_map:{kind}:{worker}` | HASH | One worker's in-flight event bodies |
| `{ns}:event-handler:in_flight_queue:{kind}:{worker}` | ZSET | One worker's in-flight leases scored by pop time |
| `{ns}:event-handler:in_flight_map:{kind}` / `in_flight_queue:{kind}` | HASH / ZSET | Legacy un-owned in-flight entries (old clients; reaped by age) |
| `{ns}:event-handler:workers` | ZSET | Worker registry scored by last heartbeat |
| `{ns}:event-handler:cache` | HASH | Cache staleness flags and generation counters |
| `{ns}:hunts:{hunt}:lane:{lane}:seq` | STRING (7 day TTL) | One lane's checkpoint sequence — the compare-and-set term |
| `{ns}:hunts:{hunt}:persist` | STRING (30 s TTL) | The per-hunt entity persist mutex |

**Scylla** — no new tables; everything rides the shared generic stores, so `thoradm` backups cover it automatically:

| Table | Holds |
|-------|-------|
| `{ns}.entities` (+ its id/name views) | `YaraRule`, `SigmaRule`, `RuleSet`, `Hunt`, `YaraRuleHit`, and `Flag` rows, one per group |
| `{ns}.associations` | `CreatedBy`, `YaraRuleHit`, `RuleSetMember`, and `HuntHit` rows |
| `{ns}.tags` | File tags (`YaraRuleHits`, `YaraScanSkipped`) and entity tags (`YaraRuleStatus`, `HuntStatus`, `HuntActive`, `HuntEngine`, `HuntId`, and the intrinsic rule/hit tags) |

**S3** — one new bucket. The event handler holds no S3 credentials: all cache traffic goes through the admin API
routes, and sample bytes come from the normal files bucket through the API's download path.

| Bucket | Object key | Notes |
|--------|-----------|-------|
| `thorium.yara.cache_bucket` (default `thorium-yara-rulesets`) | `{engine_version}/{ruleset_hash}` | Serialized compiled rulesets; roughly one live object per engine version since uploads prune same-version siblings |
