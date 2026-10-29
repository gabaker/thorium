# Risks and Open Questions

---

## Risks

### Trigger loops through hit tags

Sample tags re-enter the trigger system — `TagType::Files` tags emit `NewTags` events
(`api/src/models/tags.rs:52-57`, `api/src/models/backends/db/tags.rs:103-107`) — and the
max-depth guard is the **only loop breaker** (`event-handler/src/libs/cache.rs:150-164`;
default 5, `api/src/conf.rs:1999-2015`). A `YaraRuleHits` tag written at the default
`trigger_depth` of 0 (`api/src/models/tags.rs:270`) resets lineage depth on every hit,
letting a trigger + unpacker pair whose output re-matches a rule loop forever.
Mitigation: `YaraSampleScanEvent` carries the originating depth and `YaraHitWriter`
stamps `event.depth + 1` on every tag it writes (see [Scan Triggering](./triggering.md));
the invariant must hold for any future writer of sample tags.

### Duplicate hits under retry and multiple replicas

The dedupe inherited from sigma is a read-then-write association-tree walk
(`event-handler/src/libs/workers/hunts/sigma.rs:604-653`), and redelivery stays real
after M3's ownership work — it is just scoped and rarer: a worker's startup and
3-minute-retry resets replay its **own** uncleared events (`reset_worker` replaced both
whole-kind `reset_all` call sites), the reaper redelivers events whose owner stopped
beating (`worker_expiry_secs`) or whose lease outlived `in_flight_backstop_secs`, and
both can fire on a worker that is alive but stalled — the same event then processes
twice (see [Scan Execution](./execution.md)). Mitigation: deterministic hit identity —
UUIDv5 over `rule_id` + `sha256` with create-as-merge — makes every writer converge on
one entity whose group-merge commutes (see [Hits and Attribution](./hits.md)); clears of
reclaimed ids are no-ops; and duplicate *reactions* under reclaim are the same bounded
duplication a single-replica crash replay has always produced — reaction idempotency
keys stay out of scope.

### Heartbeat false death reclaiming live work

Death is inferred, never known: a replica whose last beat is older than
`worker_expiry_secs` (default 90s) is reaped, so a live-but-stalled worker — a Redis
partition, node pressure, a dead heartbeat task — can have its in-flight events
reclaimed and processed twice while its late clears become no-ops against moved entries.
Mitigation: beats come from a dedicated tokio task
(`event-handler/src/libs/heartbeat.rs`), never the controller loop that blocks on
multi-second compiles; the controller monitors the heartbeat task's handle and restarts
it — or exits so the pod restarts — rather than running beatless; `Events::validate()`
refuses configs where the expiry is under 3× the heartbeat interval or the age backstop
does not exceed the expiry; and a wrong call costs only the bounded double-processing
above — yara hits converge, clears no-op, reactions duplicate no worse than a crash
replay (see [Scan Execution](./execution.md)).

### Mixed-version windows during rolling upgrades

Every request load-balances independently across mixed API replicas, so a pop and its
matching clear can be served by different API versions — a clear that acts on the wrong
key family no-ops and the already-processed event replays when a backstop fires
(duplicate reactions, since reaction creation is not idempotent). Mitigation, by
direction: owned clears also delete the same ids from the legacy keys, closing the
pop-via-old / clear-via-new direction outright; the reverse direction (owned claim,
legacy clear) is bounded by `in_flight_backstop_secs` and avoided entirely by rolling
the API deployment before the event-handler deployment. Two related windows share the
same bound: the scoped reset uses a dedicated route that 404s on an old API — the
handler warns and skips rather than silently degrading to the whole-kind `reset_all`
that would replay every peer's in-flight events — leaving its own leftovers to the
legacy age gate; and reap pass 3 re-queues *every* legacy in-flight event older than the
backstop on every tick, which recovers events stranded by dead old pods but can also
steal pages a live old pod is still processing when the backlog is deeper than the
backstop (each steal is the same bounded duplication as a crash replay). All of it is
transient: once the fleet is on one version only the owned paths run.

### Revocation leaving stale hit visibility

`SubmissionUpdate.remove_groups` deletes submission rows today
(`api/src/models/backends/db/files.rs:949-959`) but cascades nothing, and entity delete
prunes only tags and entity rows (`api/src/models/backends/db/entities.rs:547-590`):
unhandled, a group whose sample access was revoked keeps seeing "sha256 X matched rule R"
forever. Mitigation: `YaraScanTrigger::GroupsRemoved` events shrink hits to the current
intersection (entity update supports group removal,
`api/src/models/backends/db/entities.rs:392-503`), and a reconciliation sweep backstops
missed events (see [Visibility and Partitioning](./visibility.md)).

### Per-group kill switches suppressing hits

`Group::authorize_check_allow_all` binds even admins to per-group `GroupAllowAction`
switches (`api/src/models/backends/groups.rs:671-677`), and entity create authorizes all
groups in one call (`api/src/models/backends/entities.rs:173-182`): one group with
`Entities` disabled would 401 the whole write and drop the hit everywhere — sigma's
silent-drop mode (`event-handler/src/libs/workers/hunts/sigma.rs:672-683`) via the
service account. Mitigation: allowability is a term of
`write_groups = rule.groups ∩ event.groups ∩ allowable_groups`, suppressions are counted
in `hits_suppressed_by_policy`, and a group disabling `Entities` deliberately opts out
of hits (see [Visibility and Partitioning](./visibility.md)).

### Silent coverage gaps

Events could vanish without trace — the owner-less legacy pop path moves events
queue → in-flight in two separate Redis pipelines (`db::events::pop`), a window M3's
atomic Lua pop closes for owned pops — and oversize samples, deleted-sample 404s, and a
missed cache flag are all silent skips: together they re-create the "did my rule run?"
expectation trap. Mitigation: `YaraScanSkipped=oversize:<cap>` markers make deliberate
skips explicit, and a periodic context rebuild backstops missed flags
(see [Scan Triggering](./triggering.md),
[Cache Invalidation and Freshness](./cache.md)). A transient loss leaves no per-sample
trace — with the `YaraScanned` ledger removed, a lost scan reads exactly like a no-match
on that sample, an accepted residual since nothing ever consumed the per-scan record (see
[Open Questions](#open-questions)). Detection is aggregate (queue depths, worker stats);
repairing a real gap is a manual hunt — nothing rescans automatically (see
[Hunts and Rule Sets](./hunts.md)).

### Slow-rule denial of service

A rule with catastrophic regex or loop behavior that compiles fine and rarely matches
never trips hit-rate quarantine yet drives every scan to the 60s `scan_timeout`; the
global ruleset offers no isolation, and timeouts poison the 3-minute retry loop
(`event-handler/src/libs/workers.rs:331-378`) for all samples. Mitigation: time-based
quarantine ships at M2 — yara-x `rules-profiling` names the slowest rules, which are
tagged `YaraRuleStatus=Slow` and dropped at the next rebuild
(see [Scaling and Performance](./performance.md)).

### Memory pressure from in-memory scanning

The default scan path holds whole samples in RAM (see
[Scan Execution](./execution.md)) and submissions record no uncarted size
(`api/src/models/files.rs:1958-1979`), so a burst of large samples could OOM-kill a
scanner that admitted them naively. Mitigation: the byte-budgeted admission scheme — a
global `scan_buffer_budget`, small-class reservations with a bounded number of
large-class upgrades, and `max_scan_size` enforced while the buffer fills — bounds
resident memory by configuration rather than by the sample mix, and the optional
`spill_dir` mode exists for deployments that would rather spend node-disk I/O and wear
than RAM (tmpfs spill is still pod RAM and buys no relief — see
[Scan Execution](./execution.md)).

### Rule-compat gaps against legacy YARA 4.x

yara-x is stricter than libyara (escape sequences, module/field gaps, edge syntax), so
parts of existing rule corpora will not import unchanged. Mitigation: create and update
return the real compiler's errors at submission time, the `thorctl rules import` bulk
importer emits a per-file compat report during import, and the legacy yara tool image
keeps running throughout (see [Engine Choice](./engine.md)).

### Global recompile latency under rule churn

yara-x cannot remove rules incrementally, so every mutation forces a full recompile; at
10^5 rules a naive rebuild-per-change would overlap itself. Mitigation: rebuilds are
debounced for at least the measured compile time and happen off the scan path (workers
swap contexts on broadcast), and M3's serialized cache lets restarts and replicas skip
compilation (see [Ruleset Compilation and Caching](./ruleset.md)).

### Engine upgrades changing match behavior

yara-x match semantics and its serialized format are not stable across crate versions —
an upgrade can change what fires. Mitigation: every hit records the pinned
`engine_version`, the serialized cache is keyed `(engine_version, ruleset_hash)`, and an
engine upgrade is treated as a rule edit for compile validation — rules that stop
compiling quarantine with a notification — while match-behavior repair over history is a
documented manual hunt, deliberately not automatic (see [Engine Choice](./engine.md),
[Hunts and Rule Sets](./hunts.md)).

### Admin-write invariant concentrated in one code path

The scanner writes as the operator-provisioned admin, so API-side group checks do not
constrain it. Mitigation (landed with M3d): `POST /api/entities/yara/hit/` recomputes
`write_groups = rule.groups ∩ event.groups ∩ sample.groups ∩ allowable_groups` from
authoritative DB state — the route verifies the rule's kind and groups via the id probe,
binds against the sample's *current* groups, filters deleted groups explicitly (group
detail casts default missing data to allow-all, so existence is checked first), and
snapshots kill switches fresh — then runs the whole partitioned write in one audited
place, collapsing `YaraHitWriter::write_hit` to a single idempotent call. The generic
create path also trims `YaraRuleHit` groups to the rule's authoritative groups whenever
the rule exists, so a stale or buggy admin writer narrows instead of leaking there too.
Residuals: other entity kinds stay unconstrained for admin writers; skip tags and
revocation shrink still compute allowability client-side; concurrent first creates of
one identity across api replicas can insert same-id rows with divergent `created`
timestamps (reads fold them, but a later full-revocation delete keys on `created` and
can orphan the stray row — the deferred reconciliation sweep repairs that remainder);
and a retried call can duplicate auto-flag entities, which stay additive by design (see
[Visibility and Partitioning](./visibility.md)).

### Event-queue backlog when scanners fall behind

Scanning is pull-based, so a throughput shortfall accumulates in Redis instead of
shedding load — invisible until results go stale. Mitigation: `GET /api/events/depths/`
reports per-kind queued and in-flight depths plus the live worker count in one call, and
every replica logs those depths each heartbeat tick; the response is then a knob, not a
redesign — raise `spec.components.event_handler.replicas`
(`operator/src/k8s/crds.rs:279-282`), with the ownership machinery keeping N replicas
safe against the shared queues (see [Scaling and Performance](./performance.md),
[Scan Execution](./execution.md)). Hunt chunks ride their own per-engine chunk queues
and budgets (`YaraHuntChunk` at M4), so a hunt backlog never queues ahead of live scan
events (see [Hunts and Rule Sets](./hunts.md)).

### Hunt-driven reaction bursts

Hunt hit tags land at depth 1 and fire tag triggers across an entire corpus of history;
the M3 live acceptance already watched an ordinary scan burst mass-create default-toolbox
reactions through hit-tag triggers (see
[Whats Left To Do](./whats_left_to_do.md#completion-notes)), and auto-flag rules inside a
hunt multiply `Flag` entities the same way. Mitigation: firing triggers from hunt hits is
deliberate — hunts feeding pipelines is a feature — and the hit-tag rate is bounded by
the hunt pacing knobs (`inter_chunk_pacing_secs`, `chunk_size`, `lanes` — the rate
formula in [Scaling and Performance](./performance.md)) plus the
active-hunt quotas; the trigger-scope review stays a pre-production item, and a
"quiet hunt" opt-out is an open question below (see [Hunts and Rule Sets](./hunts.md)).

### Checkpoint races forking lane chains

Duplicate chunk delivery is routine — the 3-minute retry, the reaper, and the age
backstop all redeliver — and a read-modify-write sequence check against the hunt entity
row is last-write-wins across API replicas (`db::entities::update` has no LWT): both
copies of a duplicated checkpoint would pass it and both would emit the next chunk event,
forking the lane chain permanently. Mitigation: the checkpoint route commits through a
per-`(hunt, lane)` Redis Lua compare-and-set that increments the sequence and emits the
next chunk event in the same script (the pop script precedent,
`api/src/models/backends/db/events.rs:37-61`); exactly one committer wins, losers 409 and
clear their stale event, and the persisted entity view is written under a short per-hunt
mutex so concurrent lane checkpoints cannot lose each other's whole-blob rewrites. A lost
Redis lane key reseeds from the entity's last persisted sequence at the cost of one
replayed, idempotent chunk (see [Hunts and Rule Sets](./hunts.md)).

### Lane imbalance

Lanes are equal time shards, but corpora skew recent, so the newest lane can carry most
of the data and lanes finish at very different times — progress per lane stays monotonic
but hunt-wide progress is lumpy. Mitigation: lanes exist for parallelism and bounded
checkpoint contention, not balance; census-weighted lane splitting is explicitly deferred
(see [Whats Left To Do](./whats_left_to_do.md)).

### Counter divergence under replay

A chunk replayed after an unpersisted checkpoint re-counts its scanned/matched work, and
a losing duplicate's deltas are dropped — hit writes converge, counters do not.
Mitigation: counters are documented as operational telemetry that can drift in both
directions; the `HuntId` tag browse is the exact record of what a hunt found (see
[Hunts and Rule Sets](./hunts.md)).

### Dangling hunt references after delete

Entity delete prunes only the deleted entity's own tags and rows
(`api/src/models/backends/db/entities.rs:547-590`), so deleting a hunt leaves
`HuntId` tags on finding entities and `HuntHit` association rows dangling — the same
platform-wide dangling-reference gap rule and sample deletes already have. Mitigation:
documented; workers treat a 404 hunt as a cancel so the chains die; the per-lane Redis
keys expire by TTL; the already-deferred association reaper covers the rows (see
[Hunts and Rule Sets](./hunts.md)).

### Cross-group rule set membership visibility

`RuleSetMember` associations are group-scoped, so two viewers can see different
member lists of one rule set, and a hunt's frozen snapshot resolves under its *creator's*
visibility — a hunt may scan rules a later viewer of the hunt cannot see. Mitigation: hit
visibility is still bounded by the write-time intersection — a rule outside a viewer's
groups produces no hit row that viewer can see — and the frozen list in the hunt's
metadata is ids only (see [Hunts and Rule Sets](./hunts.md),
[Visibility and Partitioning](./visibility.md)).

### Hunt cost control

Quotas bound concurrency and pacing bounds rate, but nothing bounds a hunt's *total*
bytes: repeated everything-up-to-now hunts are sustained S3 egress plus scan CPU, forever
(see [Scaling and Performance](./performance.md)). Mitigation: `max_active_per_user` and
`max_active_global` plus pacing are the v1 levers — and the quota check is itself
best-effort, counted through the eventually-consistent tag census, so concurrent creates
can briefly exceed it; the depths route, lane progress, and counters make cost visible;
per-group scan quotas stay on the deferred list.

### Hunt subsets bypass live quarantine windows

Noisy and slow detection are keyed to the global scan-path context: a hunt's snapshot
excludes already Noisy/Slow-quarantined rules (Broken sources still drop at classify)
but gains no new quarantine from
hunt scans, so a pathological rule inside a hunt stays pathological for that hunt's
lifetime. Mitigation: `scan_timeout` still bounds every scan, timeouts are counted per
sample and surfaced in the hunt's bounded error list, the blast radius is confined to the
hunt's own paced budget — never the shared live ruleset — and hunts write no quarantine
tags in v1 (see [Hunts and Rule Sets](./hunts.md)).

### A hunt chassis generalized ahead of its second engine

The hunt machinery ships engine-generic with exactly one engine plugged in, so the plug
contract is a design claim rather than proven API until a second engine lands — the
classic premature-abstraction trap, where the first real second consumer discovers the
seams were drawn wrong. Mitigation: the seams were derived from two *concrete* engines'
verified code, not a hypothetical — every yara/sigma divergence behind the six plug
points is catalogued with citations in [Generic Hunt Trade-offs](./hunt_tradeoffs.md) —
and the plugs sit exactly where the yara-only draft already had natural joints (snapshot
filter, corpus walk, evaluator load, write route). The sigma engine's prerequisites are
recorded there and in the deferred list, so the second consumer arrives with its
divergences known rather than discovered.

## Open Questions

- **Coverage ledger shape — resolved by removal.** `YaraScanned=<ruleset_hash>` shipped
  at M2 as a sample tag — the cheapest shape — and churned tag rows on every ruleset
  change exactly as feared; with no consumer ever built (the backscan it fed was replaced
  by hunts) the ledger was removed outright rather than reshaped. If per-sample coverage
  ever returns, it should return as the small scan-ledger table keyed by sha256, not as
  tags.
- **Matched-data excerpts on hits.** `hit.groups ⊆ sample.groups` makes excerpts safe in
  principle, but they bloat `kind_data` and the association tree. Decide before **M2**.
- **Score derivation.** Should `score` auto-derive from a `meta:` convention the way
  sigma maps `level` (`api/src/models/entities/rules/sigma.rs:157-168`)? Decide before
  **M0** ships.
- **Quiet hunts.** Should a hunt be able to suppress tag-trigger reactions and/or
  auto-flag actions for its hits (a "quiet hunt" flag)? v1 always fires both. Decide
  before **M4b**.
- **Hunt retention.** Completed and cancelled hunts persist as entities forever; given
  the dangling-reference semantics on delete, is a pruning policy needed? Decide before
  **M4d**.
- **Sigma finding identity.** The sigma hunt engine needs a convergent finding record to
  decorate — likely a new hit entity keyed `(rule_id, result key)` since the scanned
  entities themselves have no stable identity. Decide with the sigma engine milestone
  (see [Generic Hunt Trade-offs](./hunt_tradeoffs.md)).
- **Empty-intersection owner signal.** Count-only visibility of cross-group hits for rule
  owners needs a leak review before any form of it ships. Decide before **M3**.
- **Group-deletion reconciliation scope.** Hit rows only, or tags and association rows
  too? Decide before **M3**.
- **Quarantine thresholds and authority.** Noisy/slow thresholds, and who may clear
  `YaraRuleStatus` tags. Decide before **M2**.
- **Sigma adoption.** Whether sigma takes the service-account writer, quarantine,
  update-time validation, and `strum::EnumString` parsing (its `FromStr` rejects two of
  four variants, `api/src/models/entities/rules/sigma.rs:45-51`). Anytime; adjacent work.
