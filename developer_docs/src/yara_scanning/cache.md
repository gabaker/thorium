# Cache Invalidation and Freshness

---

The scanner is only ever as fresh as its `YaraRuleContext`, and that context is rebuilt
by the event-handler's controller, not per scan. This page covers how the controller
learns its caches are stale today, why that mechanism cannot carry rule entities as-is,
and the per-component scheme that replaces it.

## Today

Cache staleness is one boolean. Mutations set the single `status` field of the Redis
hash `{ns}:event-handler:cache` (`api/src/models/backends/db/keys/events.rs:31-36`):

- pipeline create and trigger updates (`api/src/models/backends/db/pipelines.rs:41`,
  `:242`)
- group create and update (`api/src/models/backends/db/groups.rs:81`, `:408`)
- user create, token refresh, and update (`api/src/models/backends/db/users.rs:46`,
  `:367`, `:620`)

The API read it back — optionally resetting it — as a single-boolean
`EventCacheStatus { triggers }`: one bit serving three unrelated producers. The
controller's refresh loop only ever rebuilt the trigger cache from it, and pushed the
`ReactionTriggers` update to workers even when nothing was rebuilt.

Entity CRUD set no flag at all — before M1 there was no cache-status write anywhere in
`api/src/models/backends/db/entities.rs` (every CRUD path bumps one today: create
`:121`, update `:502`, delete `:588`). With the shared worker cache built exactly once
at startup, sigma rules *and* the user map were frozen for the life of the process. The
sigma worker's disabled-user workaround is a symptom of exactly this: a user disabled
after startup still masquerades until restart, so stale-user 401s are detected and
skipped rather than poisoning the retry loop
(`event-handler/src/libs/workers/hunts/sigma.rs:1163-1178`). A yara ruleset frozen at
process start would be unacceptable — rule edits must reach running scanners.

## Per-Component Flags

**Decision: split the boolean into one flag per cached component**, each its own field
in the same Redis hash:

```rust
/// The current status of our event cache
pub struct EventCacheStatus {
    /// Whether the reaction trigger cache is stale
    pub triggers: bool,
    /// Whether the user map is stale
    pub users: bool,
    /// Whether the sigma rule context is stale
    pub sigma_rules: bool,
    /// Whether the yara rule context is stale
    pub yara_rules: bool,
}
```

- Producers set their own field: `db::entities` create/update/delete set `yara_rules`
  when `kind == EntityKinds::YaraRule` (and `sigma_rules` for sigma rules — the same gap,
  fixed in the same place); user mutations set `users`; pipeline and group mutations keep
  setting `triggers`.
- `reset=true` clears all fields at once — an additive change to the existing admin
  cache-status route (`api/src/routes/events.rs:399-422`).
- `rebuild_stale` checks each flag, rebuilds only what is stale, and broadcasts only the
  matching updates — which also removes today's unconditional push. `WorkerCacheUpdates`
  gains a `YaraRulesContext` variant beside `SigmaRulesContext`
  (`event-handler/src/libs/workers.rs:29-54`), and `YaraScannerWorker` subscribes to it
  exactly as the sigma worker subscribes to its context and the user map
  (`event-handler/src/libs/workers/hunts/sigma.rs:1073-1092`).

Workers keep scanning with the old `YaraRuleContext` until the broadcast swaps in the new
one — rebuilds never block the scan path.

## Generation Counters (M3)

Per-component flags fixed *what* is stale but not *who* gets told. The controller's poll
reads the hash and clears it in the same call (`db::events::get_cache_status` with
`EventCacheStatusOpts::default().reset()`), which is single-consumer by construction:
with N event-handler replicas, the first poller consumes the flag and every other
replica runs stale — old ruleset, old users, old allowances — until the slow backstop.
Four options were weighed for multi-consumer staleness:

1. **Keep read-and-clear flags.** Zero new writes, but freshness for N−1 replicas
   degrades to the ten-minute backstop — exactly the regression a horizontally scaled
   event-handler cannot ship.
2. **Per-replica flag keys.** Writers would have to enumerate the reader set, which the
   API neither knows nor wants to know, and dead replicas leak keys.
3. **Redis pub/sub.** Fire-and-forget: a replica that is restarting, mid-compile, or
   partitioned misses the publish, and there is no replay.
4. **Monotonic per-component generation counters, compare-don't-clear.** Writers bump;
   each reader remembers what it last saw. No consumer coordination at all.

**Decision: add `triggers_gen` / `users_gen` / `sigma_rules_gen` / `yara_rules_gen` to
the existing `{ns}:event-handler:cache` hash, dual-written — every site that `HSET`s a
staleness flag true also `HINCRBY`s its counter** (`db::events::set_rules_stale` plus
the pipeline, user, and group writer sites in `api/src/models/backends/db/`). The
controller polls **without** reset, compares against a `last_gens` snapshot taken before
its initial cache build (so a bump racing startup is never missed), rebuilds exactly the
components whose counters moved, and stores the new snapshot
(`rebuild_stale`, `event-handler/src/libs/controller.rs`). The boolean flags stay in the
hash and writers keep setting them, so old handlers keep their read-and-clear semantics
against a new API.

The rolling-upgrade story extends the existing `cache_status_serde_defaults` guard: the
four counters are `#[serde(default)] u64` fields on `EventCacheStatus`, so a new handler
reading an old API sees zeros that never move and degrades to the unconditional backstop
rebuild below — the same bound the system already accepts for a lost flag.

One writer site is new rather than dual-written: entity **tag** writes on rule kinds
also bump the matching rule generation, so a `YaraRuleStatus` quarantine tag written by
one replica reaches every peer's ruleset within a poll interval instead of the backstop
(see [Scaling and Performance](./performance.md)).

## The Backstop

The Scylla entity write and the Redis flag are separate systems with no transaction
between them: a crash — or a single failed `hset` — between the two leaves a rule stored
but the flag unset, and the ruleset silently freezes at its previous build. The
controller therefore also rebuilds every component unconditionally on a slow timer
(~10 minutes). A missed flag then costs minutes of staleness instead of a frozen context
that nobody notices.

## The Staleness Bound

For new samples, freshness is additive and small:

1. Rule create/update writes Scylla and sets `yara_rules` in the same request.
2. The controller polls cache status every 5s (the main loop's sleep in
   `event-handler/src/libs/controller.rs`), comparing generation counters since M3.
3. The rebuild debounce coalesces churn; it is sized at or above the *measured* compile
   time so rebuilds cannot overlap at large rule counts (see
   [Ruleset Compilation and Caching](./ruleset.md)).
4. Compile, then broadcast, swap the context under the workers.

A new rule therefore scans *new* samples within tens of seconds. Samples uploaded
*before* the rule existed are a different problem entirely: nothing in this chapter
rescans them — and nothing rescans them automatically at all. Covering history is a
hunt: a user-initiated, paced pass over a bounded corpus (M4 — see
[Hunts and Rule Sets](./hunts.md)). The asymmetry is deliberate and worth
stating plainly, because it is exactly the user-expectation trap this design exists to
close: "my rule is live" does not mean "my rule has run over everything I can see" (see
[Visibility and Partitioning](./visibility.md)). The gap is also unrecorded per sample:
only a deliberate skip leaves a marker (`YaraScanSkipped`), so a sample with no hit reads
the same whether it matched nothing or was never scanned — the `YaraScanned` ledger that
once recorded completed scans was removed as unread write volume — and confidence over a
window, like coverage itself, is a hunt (see [Scan Triggering](./triggering.md)).
