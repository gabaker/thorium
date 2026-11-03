# Operate Rule Scanning and Hunts

Thorium scans every newly uploaded sample against all stored YARA rules, scans scannable tool results against all
Sigma rules, and lets users launch hunts that re-scan historical samples with a frozen rule snapshot. All of it runs
inside components you already operate: the event-handler's workers consume the scan and hunt event queues and write
their findings back through the API. There is no separate scanner deployment to install.

This page covers the configuration knobs, what to monitor, how to respond to the common failure modes, and how to
scale and upgrade the scanning path.

> This is documentation for Thorium admins operating rule scanning and hunts. For how the scanning pipeline works
> internally, see [Rule Scanning](../architecture/rule-scanning.md). For user guides, see
> [YARA Rules](../users/yara_rules.md), [Sigma Rules](../users/sigma_rules.md), [Rule Sets](../users/rule_sets.md),
> and [Hunts](../users/hunts.md).

## Configuration Reference
---

Every knob lives in `thorium.yml` and is defaulted, so a config without these sections is still valid. The live scan
path is tuned under `thorium.yara`, hunts under `thorium.hunts`, and the shared event machinery under `thorium.events`.

### `thorium.yara`

| Field | Default | Effect |
|-------|---------|--------|
| `max_rule_size` | `131072` (128 KiB) | Max rule source size in bytes; creates/updates past it are rejected with a 400. |
| `max_scan_size` | `1073741824` (1 GiB) | Per-sample byte cap, enforced while the download buffer fills; oversize samples get a `YaraScanSkipped=oversize:<CAP>` tag instead of a scan. |
| `scan_buffer_budget` | `1073741824` (1 GiB) | Total in-flight sample bytes each replica may buffer for live scans; size pod memory to this. |
| `scan_timeout_secs` | `60` | Per-scan timeout; a timeout is a retryable failure and a slow-rule profiling signal. |
| `scan_parallelism` | unset (core count, fallback 4) | Concurrent download and scan tasks per replica. |
| `small_class_size` | `16777216` (16 MiB) | Byte-budget grant for small samples; a sample that outgrows it upgrades to a bounded large-class grant. |
| `noisy_threshold` | `0.20` | Match rate over the sliding window that quarantines a rule as `Noisy`. |
| `noisy_window` | `1000` | Sliding window length in scans (tracked per replica). |
| `slow_rule_ms` | `5000` | Scan wall time in milliseconds above which the profiler attributes slow rules. |
| `slow_strikes` | `3` | Profiler strikes before a rule is quarantined as `Slow`. |
| `allowed_modules` | `pe, elf, macho, dotnet, math, hash, string, time, lnk` | Module import allowlist enforced on rule create/update; `include` statements are always rejected. |
| `cache_bucket` | `thorium-yara-rulesets` | S3 bucket holding serialized compiled rulesets, keyed by `<ENGINE_VERSION>/<RULESET_HASH>`. |

### `thorium.hunts`

| Field | Default | Effect |
|-------|---------|--------|
| `lanes` | `4` | Parallel time-shard chains each hunt is split into. |
| `chunk_size` | `500` | Corpus list lines walked per hunt chunk. |
| `inter_chunk_pacing_secs` | `5` | Delay before a lane's next chunk becomes poppable (on top of the ~3 second event maturity floor). |
| `max_rules_per_hunt` | `10000` | Frozen rule snapshot cap; hunt creates past it are rejected with a 400. |
| `max_rules_per_ruleset` | `1000` | Rule set membership cap. |
| `max_active_per_user` | `2` | `Created`/`Running` hunts allowed per user (best-effort tag census). |
| `max_active_global` | `8` | Active hunts allowed cluster-wide (best-effort). |
| `max_error_samples` | `25` | Bounded error sample list kept in each hunt's counters. |
| `yara.scan_buffer_budget` | `1073741824` (1 GiB) | Hunt-dedicated sample buffer per replica, additive to the live `thorium.yara.scan_buffer_budget`. |
| `yara.ruleset_lru_size` | `4` | Compiled rule subsets each hunt worker holds in memory; each can be ruleset-sized. |

### `thorium.events`

| Field | Default | Effect |
|-------|---------|--------|
| `max_depth` | `5` | The only trigger loop breaker; events at or past this depth are cleared and halted. |
| `heartbeat_secs` | `10` | Heartbeat/reaper/depth-gauge tick for every event-handler replica. |
| `worker_expiry_secs` | `90` | Last-heartbeat age at which a worker is presumed dead and its in-flight events are drained. |
| `in_flight_backstop_secs` | `3600` | Lease age at which a live-but-wedged worker's in-flight events are reclaimed. |
| `stats_snapshot_secs` | `30` | Scan-stats snapshot cadence; row timestamps quantize to it, making it the cross-replica dedup unit. |
| `stats_retention` | `2592000` (30 d) | The `scan_stats` table's default TTL — how far back `GET /api/events/stats/history/` can reach. The TTL is part of the declared schema, so a change takes effect when the next schema apply alters the table: the operator reruns its schema-apply Job when a table TTL changes, or run `thoradm schema apply` yourself. It only affects rows written after that; existing rows keep their original expiry. |
| `stats_partition_size` | `43200` (12 h) | Seconds per `scan_stats` partition (1,440 rows per partition at the default cadence). |

Config load fails fast on invalid combinations: both `thorium.yara.scan_buffer_budget` and
`thorium.hunts.yara.scan_buffer_budget` must be at least `thorium.yara.max_scan_size` (hunts admit samples under the
live size cap); `thorium.hunts.lanes`, `chunk_size`, and `max_rules_per_hunt` must be at least 1;
`thorium.events.heartbeat_secs` must be at least 1, `worker_expiry_secs` at least 3x `heartbeat_secs`, and
`in_flight_backstop_secs` greater than `worker_expiry_secs`; `stats_snapshot_secs` must be at least
`heartbeat_secs` (the snapshot trigger rides the beat), `stats_partition_size` at least `stats_snapshot_secs`, and
`stats_retention` greater than `stats_snapshot_secs` and at most 630720000 (Scylla's 20 year table TTL cap).

> Sigma scanning has no config block at all — it is the older scanning path and has no size caps, budgets, or
> quarantine. The Thorium CRD only exposes event-handler scaling under `spec.components.event_handler` (`replicas`,
> default 1, and `resources`, default 2000m CPU / 4096 MiB); every knob above lives in `thorium.yml`, not the CRD.

## Monitoring
---

### Queue Depths

`GET /api/events/depths/` (admin only) returns the pending and in-flight depth for every event kind plus the number
of event-handler workers with a live heartbeat:

```JSON
{
  "kinds": {
    "YaraScannableSample": { "queued": 0, "in_flight": 12 },
    "YaraHuntChunk": { "queued": 4, "in_flight": 1 },
    "SigmaScannableResults": { "queued": 0, "in_flight": 0 },
    "ReactionTrigger": { "queued": 7, "in_flight": 0 }
  },
  "workers": 2
}
```

`in_flight` sums every worker's claimed events plus the legacy un-owned queue. Each replica also logs the same
numbers as an `Event queue depth` gauge every heartbeat tick.

### Worker Stats

Each worker publishes its cumulative counters on every rate-floored 5 second stats tick — even when unchanged — to
TTL'd Redis keys through `POST /api/events/stats/workers/<WORKER>/<KIND>/`, so throughput is queryable instead of
dying with the log stream: `GET /api/events/stats/` returns the live per-kind sums plus the raw per-worker reports
(a wedged-but-still-beating worker shows frozen counters there instead of silently vanishing), and
`GET /api/events/stats/history/` serves the persisted snapshot time series behind the
[Hunt Status page](./hunt_status.md). Counter keys expire on the `worker_expiry_secs` clock, so a dead worker ages
out of the stats view exactly when the reaper would declare it dead. Counters are cumulative since process start: a
restart resets them to zero and event replay can inflate them, so consumers derive rates between readings and clamp
negative deltas to zero. The same totals still land as `INFO` tracing events (change-gated). The counter names per
worker (nested sigma stats flatten to dotted paths like `total.scanned` and `by_entity.<KIND>.scanned`):

| Worker | Fields |
|--------|--------|
| `worker="YaraScannerWorker"` | `scanned`, `matched`, `hits_written`, `hits_suppressed_by_policy`, `hits_dropped_missing`, `skipped_oversize`, `skipped_missing`, `shrunk`, `errors` |
| `worker="YaraHuntWorker"` | `chunks`, `listed`, `scanned`, `matched`, `hits_written`, `hits_suppressed`, `hits_undecorated`, `skipped_oversize`, `skipped_missing`, `errors`, `checkpoints_won`, `checkpoints_lost`, `chains_dropped` |
| `worker="ReactionTriggerWorker"` | `seen`, `triggered`, `errors` |
| `worker="SigmaRuleworker"` | `scanned`, `created`, `flagged` totals plus a per-entity-kind breakdown |

> The sigma worker's name is spelled `SigmaRuleworker` with a lowercase `w` — grep for that exact casing.

### Log Lines Worth Grepping

| `msg=` | Meaning |
|--------|---------|
| `Event queue depth` | Per-kind `queued`/`in_flight` plus the live worker count, every heartbeat tick. |
| `Reaped event handler workers` | A reap pass reclaimed something (`dead_workers`/`reclaimed`/`backstopped`/`legacy_reclaimed`); passes that move nothing log no line, so its absence on an idle cluster is normal. |
| `Loaded yara ruleset from the serialized cache` | A rebuild or restart skipped the compile by downloading a cached ruleset. |
| `Failed to get cached yara ruleset` / `Failed to deserialize cached yara ruleset` | The ruleset cache degraded; the replica compiled locally instead (non-fatal). |
| `Quarantining yara rule` / `Quarantining yara rule that failed to compile` | A rule was tagged with a `YaraRuleStatus` of `Noisy`, `Slow`, or `Broken`. |
| `Yara scan failed` | A scan errored or timed out; a high `slow=` count points at slow rules. |
| `Server suppressed a hit our cached view allowed` | A replica's cached visibility lagged the server; the server stayed authoritative. |
| `Committed hunt checkpoint` | A hunt chunk committed (`status`/`emitted`/`exhausted`). |
| `Checkpoint lost its CAS; dropping chain` | A duplicate hunt chunk collapsed — expected under replay; chains never fork. |
| `Dropping hunt chain` | A chunk was discarded (`reason=` hunt is terminal, lane does not exist, or stale chain sequence). |
| `Heartbeat task exited; restarting this process` | The heartbeat died, so the replica exits for Kubernetes to restart it. |
| `Scoped reset route missing; skipping reset until the API upgrades` | The event-handler is newer than the API (see [Upgrades](#upgrades)). |

### Tracing

Traces land in Jaeger under the service names `ThoriumEventHandler` and `ThoriumAPI`. The spans worth searching on
the event-handler side are `YaraScannerWorker::process`, `YaraHuntWorker::process_chunk`, `YaraRuleContext::new`,
`YaraRuleContext::for_subset`, `YaraHitWriter::write_hit`, and `Heartbeat::beat`; on the API side
`Entity::create_yara_hit`, `Entity::create_hunt`, `Entity::cancel_hunt`, `Entity::checkpoint_hunt`,
`db::hunts::commit_lane`, and `db::events::set_rules_stale`.

### Admin Event Routes

All of the routes below require admin credentials; the event-handler's service account is their main caller.

| Method | Route | Description |
|--------|-------|-------------|
| `GET` | `/api/events/depths/` | Queue depths per event kind plus the live worker count |
| `GET` | `/api/events/cache/status/` | Cache staleness flags and generation counters |
| `PATCH` | `/api/events/pop/<KIND>/` | Pop events of one kind into the caller's in-flight set |
| `DELETE` | `/api/events/clear/<KIND>/` | Clear processed events |
| `PATCH` | `/api/events/reset/<KIND>/` | Move the legacy un-owned in-flight events of one kind back to pending (current workers' claims are untouched — use the per-worker reset or drain routes for those) |
| `PATCH` | `/api/events/workers/reset/<WORKER>/<KIND>/` | Move one worker's in-flight events back to pending |
| `PATCH` | `/api/events/workers/heartbeat/<WORKER>/` | Upsert a worker's registry heartbeat |
| `DELETE` | `/api/events/workers/<WORKER>/` | Drain a worker, requeueing everything it had in flight |
| `POST` | `/api/events/workers/reap/` | Run a reap pass over the worker registry |
| `POST` | `/api/events/stats/workers/<WORKER>/<KIND>/` | Upsert one worker's TTL'd cumulative counter report |
| `GET` | `/api/events/stats/` | The live scan stats aggregate: per-kind sums, depths, and per-worker reports |
| `POST` | `/api/events/stats/snapshot/` | Persist this quantized window's scan-stats row (204 when a peer already has) |
| `GET` | `/api/events/stats/history/` | Ranged, downsampled snapshot history (`start`/`end`/`step_secs`, oldest first) |
| `GET` | `/api/entities/hunt/status/` | The hunt census: ongoing/upcoming/recent hunts, per-user usage, chunk backlog |
| `GET`/`POST` | `/api/events/yara/ruleset/<ENGINE_VERSION>/<RULESET_HASH>/` | Download/upload a serialized compiled ruleset (the S3-backed cache) |
| `POST` | `/api/entities/hunt/<ID>/checkpoint` | Commit one processed hunt chunk and schedule the next |

## Failure Modes and Remediation
---

1. **A queue keeps growing.** Event payloads are a few hundred bytes, so even a million queued events is only around
   200 MB of Redis — you have time to react. A backlog is a throughput deficit: raise
   `spec.components.event_handler.replicas`. Unscannable objects cannot pile up, since oversize and deleted samples
   clear immediately. The one exception is a slow rule dragging every scan to the timeout: throughput then tops out
   near `scan_parallelism / scan_timeout_secs` scans per second (pool size divided by 60 with the default timeout)
   and more replicas only multiply stalled pools. Let the `Slow` quarantine fire, or edit or delete the offending
   rule — do not scale first when `Yara scan failed` lines show a high `slow=` count.
2. **Quarantined rules.** The scanner tags misbehaving rules `YaraRuleStatus=Broken`, `Noisy`, or `Slow` and writes
   the reason as a notification on the rule. Find them by browsing YARA rules filtered on the `YaraRuleStatus` tag.
   Rule edits never clear status tags — to re-admit a rule, fix it and then delete its `YaraRuleStatus` tag with
   `thorctl rules unquarantine <RULE>`, the **Clear quarantine** control on the rule's details page, or
   `DELETE /api/entities/tags/<ID>`; the tag delete bumps the rule generation so scanners pick it back up at their
   next rebuild. A rule that is still noisy or slow will be re-quarantined. Rules that must never be sidelined
   (e.g. a watchlist rule that legitimately matches broadly) can be **exempted** from `Noisy`/`Slow` quarantine by
   an admin with `thorctl rules exempt <RULE>` or `POST /api/entities/rules/<ID>/quarantine-exemption`: setting the
   exemption also clears any current `Noisy`/`Slow` status in the same action, `--clear` (or `DELETE` on the same
   route) removes it, and `Broken` tagging is unaffected since an uncompilable rule cannot scan regardless.
   Thorctl's `<RULE>` arguments take the rule's ID or its exact name (an ambiguous name errors with the candidate
   IDs); the API routes always take the ID. The following clip exempts a quarantined rule, then posts and retires
   an advisory notification on it:

   <div class="asciinema-clip" style="max-width: 800px; margin: 0 auto;" data-cast="../static_resources/casts/rules-admin.cast"></div>
3. **Mass-reaction storms.** Hit tags (`YaraRuleHits=<RULE_NAME>`) are plain file tags, so they fire existing
   tag-based reaction triggers — one large import or hunt on a trigger-heavy cluster can spawn hundreds of reactions.
   Review your pipelines' trigger scope before big rule imports and hunts. Chains stop at `thorium.events.max_depth`
   (5), and hunt bursts are bounded by pacing and the active-hunt quotas.
4. **Rule edits not taking effect.** An edit normally reaches live scanning within tens of seconds (a 5 second cache
   poll plus a rebuild debounce of at least 30 seconds plus compile time). The worst case for a lost staleness bump
   is the unconditional 10-minute full rebuild backstop. `Server suppressed a hit our cached view allowed` warnings
   mean a replica's cached view lagged the server; the server stayed authoritative and nothing leaked.
5. **Dead or wedged replica.** The reaper drains a dead worker's in-flight events within `worker_expiry_secs`
   (90 seconds) plus one tick; a live-but-wedged worker's events are reclaimed once their leases age past
   `in_flight_backstop_secs` (3600 seconds). Both produce bounded duplicate processing: hit writes converge, but
   duplicate reactions are possible.
6. **A hunt is stranded in `Created`.** If a hunt sits in `Created` with no queue activity, its first chunk events
   failed to emit. Cancel it and recreate it — there is no kick or resume route. A restore strands every hunt that
   was `Created` or `Running` when its backup was taken the same way, since a backup holds no queued chunk events.
7. **Deleted hunts leave dangling references.** Deleting a hunt does not sweep the `HuntId` entity tags and
   `HuntHit` associations on its hits. This is a known gap; an association reaper is deferred.
8. **Coverage questions.** `YaraScanSkipped=oversize:<CAP>` means a sample was deliberately skipped and why.
   Completed scans leave no per-sample marker, so a sample with no hit either matched nothing or was never
   scanned — to guarantee coverage of a window, run a hunt over it. Samples scanned before the coverage-ledger
   removal may still carry stale `YaraScanned` tags; they are inert.
9. **Missed revocation events.** When a sample loses a group its hits shrink or delete automatically, but a missed
   revocation event is only repaired by a reconciliation sweep that is not built yet. This is a known residual risk.

## Scaling Guidance
---

- **The bottleneck is byte movement, not matching.** yara-x matches at roughly 100 MB/s per core; S3 reads and CaRT
  decoding dominate the scan path. Scale for download throughput, not match CPU.
- **Per-replica memory** is roughly the compiled ruleset (hundreds of MB for large rule corpora) plus
  `thorium.yara.scan_buffer_budget` plus `thorium.hunts.yara.scan_buffer_budget` plus up to `ruleset_lru_size` (4)
  compiled hunt subsets. With both budgets at their 1 GiB defaults, the default 4096 MiB event-handler pod has
  limited headroom — raise `replicas`, the budgets, and pod memory together, never one alone.
- **Hunt throughput** per hunt is about `lanes × chunk_size / (chunk duration + inter_chunk_pacing_secs)` corpus
  lines per second, and a hunt re-reads its whole corpus from S3 — nothing bounds a hunt's total bytes. The
  active-hunt quotas are the lid on concurrent hunt load.
- **Noisy detection is per replica.** Each replica keeps its own `noisy_window`, so with N replicas a noisy rule
  takes roughly N times as long to quarantine; once one replica tags it, every replica drops it at its next rebuild.

## Upgrades
---

Roll the API before the event-handler. A newer event-handler retries hunt checkpoint 404s from an older API and
skips owner-scoped resets with a `Scoped reset route missing` warning, and events popped by a new worker but cleared
through an old API replay after the lease backstop — all bounded, but running a mismatched event-handler against a
different API build can crash-loop at startup while building its rule context.

The scan engine is pinned to `yara-x =1.21.0`. Every hit records the `engine_version` that produced it, and the
serialized ruleset cache is keyed by `(engine_version, ruleset_hash)`, so a new engine never reads stale compiled
objects. An engine upgrade can change match behavior and quarantine rules that no longer compile; hits written under
the old engine stay as they are. To refresh history after an engine upgrade, run a hunt over the window you care about.

## Deferred / Not Built
---

- **Sigma hunts** — hunts only support the YARA engine today.
- **Repo scanning** — rules apply only to samples; a `Repos` hunt corpus is rejected at create.
- **A hunt kick/resume route** — cancel and recreate is the only remedy for a stranded hunt.
- **Spill-to-disk scanning** — sample bytes are buffered in memory only.
- **An association reaper** — deleted hunts leave dangling `HuntId` tags and `HuntHit` associations.
- **Per-sample scan-coverage records** — only deliberate skips are tagged; nothing records "this sample was
  scanned with build X" (the write-only `YaraScanned` ledger was removed).
- **Per-group scan quotas** — nothing bounds the total bytes a hunt reads.
