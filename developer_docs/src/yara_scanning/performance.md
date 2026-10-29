# Scaling and Performance

---

The cost model is deliberately simple: one global compiled ruleset, one pass per sample,
hits partitioned at write time. This page quantifies that model, shows where the real
bottleneck is, and defines the two quarantine mechanisms that protect a shared ruleset
from its worst rules.

## The Model

Let `U` = sample uploads/day, `S` = mean sample size in MB, `R` = loaded rules, and `T` =
yara-x throughput in MB/s/core. Because every rule compiles into one ruleset and the
engine shares its atom-matching pass across all of them (see
[Ruleset Compilation and Caching](./ruleset.md)), scan cost is ~O(bytes scanned) and only
weakly O(`R`). Assuming a conservative `T` = 100 MB/s/core for regex-heavy rulesets:

| `U` | `S` | Bytes scanned | Sustained scan cores |
| --- | --- | --- | --- |
| 100k/day | 2 MB | 200 GB/day | ~0.02 |
| 1M/day | 10 MB | 10 TB/day | ~1.2 |

**Bytes movement, not matching, is the bottleneck.** The same 200 GB/day must stream out
of S3 and CaRT-decode before a single rule evaluates; matching CPU is a rounding error
next to the transfer and decode work. Mitigations: download concurrency is decoupled from
scan threads (see [Scan Execution](./execution.md)), and M3 scales event-handler
replicas — each with its own download pool — near S3. These numbers are also the
quantitative case for the global ruleset — per-group rulesets would re-walk the same
bytes once per group set — and against per-job containers, whose per-sample startup
overhead dwarfs the median scan.

## Memory, Not Disk

The scan path never spools samples to local disk (see [Scan Execution](./execution.md)):
bytes stream from S3 through the CaRT decoder into a RAM buffer, are scanned once with
`Scanner::scan`, and dropped. A scratch-file design would write every scanned byte to
disk just to read it straight back — at the 1M/day row above that is **10 TB/day of pure
scratch writes** on the scanner nodes: real SSD endurance and a second pass of local I/O
bandwidth for a buffer discarded seconds later (the read-back usually comes from page
cache anyway, so the write is all cost and no benefit). Memory-only scanning removes it
entirely, and what the operator sizes instead is bounded and explicit:

- Resident memory = the compiled ruleset (one `Arc<yara_x::Rules>` per process) +
  `scan_buffer_budget`, the byte-budgeted cap on all in-flight sample buffers
  (see [Scan Execution](./execution.md) for the admission scheme). The defaults are
  sized to the operator's default event-handler pod (4096 MiB total): `scan_buffer_budget`
  and `max_scan_size` both default to **1 GiB** and config load rejects
  `scan_buffer_budget < max_scan_size`; raise them together with the pod's memory.
- The optional `spill_dir` mode re-introduces files only where an operator explicitly
  chooses node-disk wear over RAM — mmap pages are evictable where budget bytes are
  not. tmpfs spill is not that trade: it is still pod RAM, just outside the budget
  (see [Scan Execution](./execution.md)).
- **Replicas multiply, they do not share** (M3). Each event-handler replica holds its
  own compiled ruleset `Arc` and its own `scan_buffer_budget`, so cluster resident
  memory is `N × (ruleset + scan_buffer_budget)`. That is deliberate — a shared budget
  would put cross-process coordination on the hot path — but it means
  `spec.components.event_handler.replicas` (`operator/src/k8s/crds.rs:279-282`) and the
  per-pod memory request are raised together, never one without the other.

## Compile Economics at N Replicas

Without a cache, every replica pays the full combined compile on every rule change and
on every restart: one edit costs N compiles, and a rolling update recompiles the world
N times for nothing. The M3 serialized-ruleset cache changes the slope: the first
replica to build a given `(engine_version, ruleset_hash)` uploads the serialized rules
through the admin cache routes, and every other replica — and every restart — downloads
and `deserialize`s instead of compiling (see
[Ruleset Compilation and Caching](./ruleset.md)). Because rebuilds are already debounced
by measured compile time, a rule change costs roughly **one compile cluster-wide**;
everything else is an S3 GET of a blob the handler streams anyway. Every cache failure
degrades to a local compile with a warning — the cache is an economics feature, never an
availability dependency.

## Write Amplification

A new hit with `G = |write_groups|` fans out across Scylla and Redis:

- Entity rows ×`G` plus a census increment — entity create inserts one row per group
  (`api/src/models/backends/db/entities.rs:55-92`).
- `YaraRuleHits` tag rows ~×`G`, plus tag census updates, a `NewTags` event, and a tag
  search event (`api/src/models/backends/db/tags.rs:49-118`).
- Association rows: 2 kinds (`AssociationKind::CreatedBy` rule→hit,
  `AssociationKind::YaraRuleHit` hit→sample) × 2 directions × `G` — the associations
  table keys on `group` with `direction` in the clustering key, so every edge is one row
  per group per direction (`api/src/models/schema/scylla/associations.rs`).

At `G = 3` that is roughly 15–20 writes per *new* hit; group merges into an existing hit
are cheaper (update rows plus tag adds). At a 1% hit rate on 100k uploads/day that is
~20k writes/day — noise for Scylla. **The real risk is rule quality, not DB load**, which
is what the next two sections police.

## Noisy Rules (hit-rate quarantine)

`YaraScannerWorker` keeps per-rule counters using the same stats shape as the sigma
worker — totals plus a keyed breakdown reported over the per-kind stats channels
(`SigmaRuleStats` and its `StaticMap`,
`event-handler/src/libs/workers/hunts/sigma.rs:29-67`; channel plumbing
`event-handler/src/libs/stats.rs:14-56`) — with the breakdown keyed by rule entity id:
`scanned`, `matched`, `hits_written`, `hits_suppressed_by_policy`, `errors`, and
`scan_time`.

A rule whose sliding-window hit rate crosses the configured threshold (e.g. matching >20%
of the last 1,000 samples) is quarantined as noisy:

- `YaraRuleContext` stops writing its hits but keeps counting its matches, so the stats
  still show what the rule would have done.
- The rule entity is tagged `YaraRuleStatus=Noisy` and the submitter is notified.
- Quarantine clears on rule edit; whether operators may also clear `YaraRuleStatus`
  tags directly is an open question (see [Risks and Open Questions](./risks.md)).

The windows are **per-replica by design** (M3). Each replica's sliding window sees
roughly 1/N of the scan stream, so the threshold still measures the same
scale-invariant *rate* — it just takes ~N× longer to fill before a noisy rule trips.
Shared Redis windows were weighed and rejected: they would add a hot-path Redis write
per scan to improve only detection *latency*, not correctness, on a rare path.
Convergence comes from the tag instead: before quarantining, a replica re-fetches the
rule entity and skips if any `YaraRuleStatus` tag already exists (one GET on a rare
path — so operators get one notification, not N), and the status-tag write bumps the
rule generation counter, so every peer drops the rule at its next rebuild within
seconds rather than at the ten-minute backstop (see
[Cache Invalidation and Freshness](./cache.md)).

Hard caps hold regardless of quarantine: at most one hit entity per `(rule, sha256)` by
dedupe identity (see [Hits and Attribution](./hits.md)), and bounded match metadata per
hit.

## Slow Rules (time quarantine)

Hit-rate monitoring has a blind spot: a rule with catastrophic regex or loop behavior
that compiles fine and almost never matches. It never trips the noisy threshold, yet it
drives *every* scan to the `conf.thorium.yara.scan_timeout` (default 60s). With one
global ruleset there is no isolation — one such rule is everyone's problem:

- Throughput collapses. With every scan running to its timeout the gate is the
  `spawn_blocking` scan pool (default: core count, see
  [Scan Execution](./execution.md)): 60s per scan caps the system at pool-size/60
  scans/s — ~0.13/s on 8 cores, and never more than the event worker's 10-way
  `buffer_unordered` page width (`event-handler/src/libs/workers.rs`) permits — below
  even the ~1.2/s that 100k uploads/day requires, so the queue grows without bound.
  Adding replicas multiplies a stalled pool, not throughput: the `replicas` knob is no
  answer to a slow rule.
- Timeouts poison the retry loop: any failed event arms the 3-minute retry
  (`EventWorker::process`, `event-handler/src/libs/workers.rs`), which replays the
  worker's own in-flight events into the same stall.
- A timeout attributes no rule, so operators cannot find the culprit without bisection.

**Decision: time quarantine ships with the scanner in M2, not in a later hardening
pass.** yara-x is built with its `rules-profiling` cargo feature; on (sampled) scans the
worker reads the slowest-rules profile, tags offenders `YaraRuleStatus=Slow`, drops them
at the next ruleset rebuild, and notifies the submitter — the same quarantine machinery
used for compile failures (see [Ruleset Compilation and Caching](./ruleset.md)). A scan
that hits its timeout is treated as a quarantine-investigation signal, not merely a
retryable error.

## Queue Health

Backpressure lives in Redis, and the queue is cheap to let grow:

- Event payloads are small — a `YaraSampleScanEvent` serializes to a few hundred bytes,
  so even a 1M-event backlog is ~200 MB of Redis.
- `GET /api/events/depths/` reports per-kind `{queued, in_flight}` plus the live worker
  count in one call — the legacy `ZCARD` plus per-owner `ZCARD`s found through the
  worker registry — and each replica's heartbeat task logs those depths as a tracing
  gauge every tick, so a growing backlog is visible well before it matters.
- The response to a sustained backlog is a knob, not a redesign: raise
  `spec.components.event_handler.replicas` (`operator/src/k8s/crds.rs:279-282`); the
  ownership machinery keeps N replicas safe against the shared queues (see
  [Scan Execution](./execution.md)).
- Pops page up to 1,000 events at a time (`EventPopOpts::default().limit(1000)`,
  `event-handler/src/libs/workers.rs`) and each page coalesces events by sha256 (see
  [Scan Triggering](./triggering.md)), so an upload burst costs one download and one
  scan per distinct sha256 per page.
- Unscannable objects never accumulate: an oversize or deleted sample clears its events
  immediately as a deliberate skip (`YaraScanSkipped` marker — see
  [Scan Execution](./execution.md)), so backlog growth can only come from throughput
  deficit, never from a population the scanner can never drain.
- Successes clear per event id (`processed_event_clears`,
  `event-handler/src/libs/workers.rs`); a failure arms a 3-minute retry that resets only
  the worker's **own** in-flight events (`db::events::reset_worker` — M3 replaced both
  whole-kind `reset_all` call sites), so one bad sample retries alone and replicas never
  replay each other's in-flight work. Events owned by a dead or wedged replica come back
  through the heartbeat reaper and its age backstop instead (see
  [Scan Execution](./execution.md)).

## Hunt Cost and Pacing (M4)

A hunt re-walks history: every sample in its corpus streams out of S3 and CaRT-decodes
again, so the bytes-movement bottleneck above applies to the *whole corpus*, not just the
day's uploads. At the first row of the cost model (100k uploads/day × 2 MB), ninety days
of history is ~18 TB of S3 egress and decode — per hunt. That is why hunts are paced,
quota'd, and never automatic (see [Hunts and Rule Sets](./hunts.md)): rescanning a corpus
is a capacity decision the operator budgets and a user makes explicitly.

- **The knobs are the system's first pacing configuration** (`conf.thorium.hunts`, with
  per-engine sub-blocks like `hunts.yara`). Sustained per-hunt scan rate is bounded by
  `lanes × chunk_size / (chunk duration + inter_chunk_pacing_secs)`, and concurrent cost
  by `max_active_per_user` and `max_active_global`. Nothing else in Thorium rate-limits
  a queue today.
- **Memory is additive.** A replica running the yara hunt engine holds the live
  `scan_buffer_budget`, the hunt `hunts.yara.scan_buffer_budget`, and up to
  `hunts.yara.ruleset_lru_size` compiled subset rulesets on top of the global ruleset
  `Arc` — raise the event-handler pod's memory together with these knobs, exactly like
  the live budget rules above.
- **Fairness is structural, not scheduled.** Hunt chunks ride per-engine chunk queues and
  workers (`EventType::YaraHuntChunk` at M4); the live `YaraScannableSample` queue is a
  strict `ZPOPMIN` with no priority mechanism, so the only way a hunt backlog cannot
  starve live scanning is to never share that queue — and it doesn't. Per-engine queues
  also keep one engine's backlog from head-of-line blocking another's (see
  [Generic Hunt Trade-offs](./hunt_tradeoffs.md)).
- **Chunk duration versus the in-flight lease is stated math, not hope.** A worst-case
  chunk — every scan running to `scan_timeout` — takes about
  `chunk_size × scan_timeout / parallelism` ≈ 125 minutes at the defaults with 4-way
  scan parallelism, past the
  1-hour `in_flight_backstop_secs`, so the age backstop can redeliver a still-processing
  chunk. The duplicate collapses at the checkpoint CAS; the cost is bounded duplicated
  scanning and lost counter deltas, never a forked lane (see
  [Hunts and Rule Sets](./hunts.md)).
- **Observability.** Hunt chunk queues report through the same `GET /api/events/depths/`
  as every event kind, and the hunt entity itself carries per-lane time-based progress
  plus counters, so a stalled or runaway hunt is visible without new machinery.
- **Reaction amplification is bounded by the same pacing.** Hunt hits write the same
  depth-stamped hit tags as live scanning, and tag triggers fire on them — deliberately.
  The hit-tag rate a hunt can sustain is the scan rate above, so pacing is also the lid
  on trigger-spawned reaction bursts (see [Risks and Open Questions](./risks.md)).
