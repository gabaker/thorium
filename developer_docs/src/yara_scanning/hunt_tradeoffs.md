# Generic Hunt Trade-offs

---

The hunt design was first drafted yara-only — `YaraHunt`, `YaraRuleSet`,
`YaraHuntId` — and then generalized to a **generic chassis with per-engine plugs**
before any of it was implemented. This page records that decision honestly: what
genericity buys, what is irreducibly engine-specific, what over-generalizing would have
degraded, what the sigma engine is actually gated on, and the costs accepted. The
generalization was assessed against the two rule engines Thorium already runs — the yara
scan path and the sigma scan path — by reading their code, not by hypothesizing a future
engine; every claim below carries its citation.

## Why Generalize at All

- **It is free right now.** M4 is design-only — no `Hunt`, `RuleSet`, or chunk-event
  symbol exists anywhere in the workspace — so generalizing costs renames on paper, with
  none of the append-only-enum or wire-compat consequences a later migration would carry
  (`EntityMetadata` variants are rkyv-archived in backups and append-only,
  `api/src/models/entities.rs:755-763`).
- **The chassis was already engine-agnostic by accident.** Walking the design piece by
  piece, nothing in the selector/snapshot machinery, corpus bounds, lanes, chunk chains,
  CAS checkpoint, pacing, quotas, status tags, counters, lifecycle, or decoration layer
  is yara-shaped (the chunk event is `{hunt, lane, seq}`, and tag/association storage
  carries no entity-kind column at all,
  `api/src/models/schema/scylla/tags.rs`). Naming it `Yara*` would
  have been a false claim of specificity.
- **Rule sets are useful beyond hunts, today.** A generic `RuleSet` groups sigma rules
  the moment it ships, independent of any sigma hunt engine.
- **A real naming collision dissolves.** The UI already exports `type YaraRuleset` as the
  `YaraRule` metadata-fields type (`ui/src/models/entities/rules/yara.ts:33`); a
  `YaraRuleSet` entity model would have collided with it up to casing and forced a UI
  type rename. `RuleSet` sidesteps it entirely.
- **The next corpus is nearly free.** `RepoListParams` is field-for-field the
  `FileListParams` shape with its own `earliest` default
  (`api/src/models/git/repos.rs:1350-1370`; table `repos_list` keyed
  `((group, year, bucket), uploaded, id)` like `samples_list`,
  `api/src/models/schema/scylla/repos.rs`) — the `HuntCorpus`
  enum's `Repos` variant activates with repo scanning rather than being designed then.
- **Future engines get a contract instead of a fork.** Six plug points (the table in
  [Hunts and Rule Sets](./hunts.md#engines)), each already exercised by the differences
  between yara and sigma.

## What Is Irreducibly Engine-Specific

The six plug points exist because the two engines we can read genuinely differ at each:

1. **Rule kind + snapshot filter.** Yara excludes `Noisy`/`Slow`-quarantined rules at
   snapshot, matching its live list-time skip
   (`event-handler/src/libs/workers/hunts/yara/context.rs:111`, `:532-534`); sigma has no
   quarantine concept at all — a broken sigma rule is logged and skipped at context build
   (`event-handler/src/libs/workers/hunts/sigma.rs:129-143`), nothing is tagged or
   notified — so its filter is a no-op.
2. **Corpus.** Yara's unit is a sample; sigma's is a *tool result*
   (`SigmaScannableResultsEvent { key, groups, applies_to, tool }`,
   `api/src/models/events.rs:117-126`) — and sigma does not scan raw output text but the
   structured entity attachments stored beside a result in S3, fetched per
   `(key, tool, result_id, kind)` (`api/src/models/results.rs:1588-1612`). Different
   unit, different fetch, different listing (below).
3. **Fetch + admission.** Yara downloads bytes under a byte budget and burns the blocking
   pool (`event-handler/src/libs/workers/hunts/yara/budget.rs:62-84`); sigma is pure
   async JSON I/O with no budget or size cap anywhere today (`EntityMap::populate`,
   `sigma.rs:751-784`). "One shared admission policy" has no meaning across those.
4. **Evaluator.** Yara needs the classify/hash/combined-compile pipeline, a serialized
   cache, and an LRU because compiles are expensive and cacheable by content hash
   (`context.rs:183/:228/:256/:367`); sigma's "compile" is a per-rule YAML parse with no
   cross-rule state, nothing to cache, and no content hash to key a cache on — `SigmaRule`
   has no `rule_hash()` at all (`rule_hash` exists only on `YaraRule`,
   `api/src/models/entities/rules/yara.rs:171-179`).
5. **Finding writer.** Yara has a deterministic convergence anchor — UUIDv5 over
   `(rule_id, sha256)` (`api/src/models/entities/hits.rs:92-101`) — behind one audited
   server route. Sigma has neither (next section). The chassis can promise *decoration*;
   only an engine can promise *what converges*.
6. **Config.** Yara's knobs exist (`conf.thorium.yara`, `api/src/conf.rs:2216-2253`);
   sigma has no config section anywhere in `conf.rs` at all.

## What Over-Generalizing Would Have Degraded

Genericity was cut at the chassis/plug seam deliberately. Three plausible "more generic"
cuts were weighed and rejected because each degrades something real:

- **One unified corpus struct instead of an enum.** Results need a `tool` filter that
  files and repos have no use for; files/repos tag filters mean nothing for results. One
  flat struct either grows nonsense fields per kind or loses per-kind filters. The enum
  beside one shared bounds shape is the `Collection` entity's proven pattern
  (`CollectionKind { Files, Repos }`, `api/src/models/entities/collections.rs:40-45`).
- **One shared chunk queue and worker for all engines.** The worker traits bind one
  worker to one `EventType` and one stats type
  (`event-handler/src/libs/workers.rs:76-110`), queue depth reporting is per kind, and
  the controller runs exactly one worker per kind per process
  (`event-handler/src/libs/controller.rs:334-357`). A merged queue erases per-engine
  depth observability and lets resource profiles head-of-line block each other: a yara
  chunk holds byte-budget grants and `spawn_blocking` CPU while a sigma chunk is cheap
  async I/O — in one worker's page the sigma chunks would queue behind budget waits they
  do not need. Per-engine event kinds cost a handful of match arms (`Display` makes the
  queue keys, `EnumIter` covers reap/drain/depths automatically,
  `api/src/models/backends/db/keys/events.rs:44-78`) and keep both isolation and the
  fairness-by-construction argument.
- **One shared finding record.** Yara findings are `YaraRuleHit` entities with scan-fact
  metadata and newer-wins refresh; sigma "findings" are heterogeneous promoted entities
  plus `Flag`s in the rule's groups. Forcing one record shape would flatten sigma's
  semantics into yara's. Instead the generic layer is the *decoration* — `HuntId` entity
  tag + `HuntHit` association, kind-blind by construction — and the record underneath is
  the engine's.
- **Mixed-engine hunts** (one hunt running sigma *and* yara) were rejected: two engines
  mean two corpora, two budgets, and two progress semantics inside one entity; "hunt this
  with both" is simply two hunts. Rule *sets* stay mixed — the hunt snapshot filters to
  its engine and counts the rest (`excluded_foreign`).
- **Per-engine hunt entity kinds** (`YaraHunt`, `SigmaHunt`, …) — the opposite error —
  were also rejected: every entity-kind touchpoint (forms, UI configs, backup enums,
  intrinsic tags) would duplicate per engine for no query the `HuntEngine` tag does not
  already answer.

One more rejection matters most: **the chassis must not accommodate sigma as it is
today.** Sigma scans through per-user masqueraded clients keyed by the event's user
(`clients: PapayaMap<String, Thorium>`, `sigma.rs:860`;
`Thorium::masquerade`, `api/src/client.rs:887`). Carrying that into the chassis would
break it in two places: lane cursors are username-scoped Redis keys
(`api/src/models/backends/db/keys/cursors.rs:19-26`), so any-replica lane resume works
*only* because every hunt worker is the one service account; and the no-leak invariant
(`write_groups ⊆ corpus groups ⊆ hunt.groups`,
[Hunts and Rule Sets](./hunts.md#hits-attribution-and-browse)) requires an
intersection-computing writer, where sigma today writes into the rule's groups verbatim
(`req.groups = rule.original.groups.clone()`, `sigma.rs:388`). The chassis therefore
*assumes* service-account engines with audited, intersecting write routes — and sigma
conforms later rather than the chassis degrading now.

## What the Sigma Engine Is Actually Gated On

Making the design generic does **not** make a sigma engine cheap. The gaps, verified:

1. **A sigma finding record does not exist.** A sigma match only drives its `actions`
   loop (`sigma.rs:1032-1049`); a matching rule with no configured actions persists
   *nothing* — no entity, no tag, and not even a stat (`SigmaRuleSpecificStats` counts
   `scanned/created/flagged`, with no `matched`, `sigma.rs:31-38`). A hunt has nothing to
   decorate for action-less rules, so a sigma hit record with deterministic identity
   (the natural anchor: keyed on `(rule_id, result key)`) must exist before "what did my
   sigma hunt find?" is answerable. Notably, `AssociationKind::SigmaRuleHit` already
   exists and is written by nothing (`api/src/models/associations.rs:94`; its only other
   reference is its own `as_str` arm, `:138`).
2. **The writer modernization — already the book's deferred "porting the fixes back to
   sigma" item** (see [Whats Left To Do](./whats_left_to_do.md)): service-account writer
   (the masquerade/cursor clash above), a server-side audited route computing the group
   intersection (sigma's entity-create failures today are logged and swallowed — the
   "silent-drop mode", `sigma.rs:672-683` — so a sigma hunt's `written` counter would
   overcount reality in a way yara's cannot), and convergent dedupe (sigma's
   read-then-write association-tree walk, `filter_reqs`, `sigma.rs:609-653`, duplicates
   under exactly the redeliveries the chunk chassis treats as routine).
3. **No results corpus exists.** The `results_stream` table has carried the right
   listing shape since inception — `((kind, group, year, bucket), uploaded DESC)` with
   `tool` as a regular column
   (`api/src/models/schema/scylla/results.rs`) — but it has
   insert/delete statements only, no census writes (`db::results::create` never calls
   `census::incr_cache`, `api/src/models/backends/db/results.rs:29-125`), no list route,
   no cursor implementation, and `tool` is in no key column. A results walk is new
   API-surface work; the cheaper alternative is walking the existing files/repos listings
   and point-fetching each item's current results — which also matches what sigma
   actually scans (the live worker always evaluates the *newest* result per tool,
   `sigma.rs:799-807`).
4. **Result retention makes the corpus shallow.** Results default to **3 retained
   versions per (key, tool, group)** (`api/src/conf.rs:56-58`, `:74-75`), pruned at every
   result create (`api/src/models/backends/db/results.rs:99`, `prune` at `:500`) — so
   "every result before T" largely degenerates to "the currently retained results".
   That inverts the economics that justified yara hunts (immutable history worth
   re-reading) and is a semantic decision to record before building anything.
5. **Smaller ports ride along**: sigma rules have no content hash (nothing to record as
   "the exact text that fired"), sigma rule *updates* skip re-validation
   (`update!(rule.rule, …)` with no parse, `api/src/models/backends/entities.rs:1017-1019`
   — the yara arm re-validates with an explicit "unlike sigma rules" comment,
   `:1040-1044`), and sigma's pipeline zeroes trigger depth at emission
   (`api/src/models/backends/events.rs:221-222`) — harmless while sigma writes nothing
   event-emitting, but a sigma hunt that ever writes a sample tag must first plumb the
   depth invariant end to end (see [Scan Triggering](./triggering.md)).

None of this blocks M4. All of it would have been *invisible* under a yara-named design
and painful to discover mid-implementation of a "quick" sigma copy.

## Costs Accepted

Genericity is not free; these are the prices paid, with eyes open:

- **One `Hunt` kind cannot be split by the `kinds[]` list param.** "My yara hunts" needs
  the `HuntEngine` tag filter instead of a kind filter — one more tag write per hunt and
  a slightly less obvious browse query.
- **The chassis ships validated by one engine.** Until a second engine lands, the plug
  contract is a design claim, not proven API — the classic premature-abstraction risk.
  Mitigation: the seams were derived from two *concrete* engines' verified code (every
  divergence above has a citation), not from a hypothetical; and the plug points are
  exactly where the committed yara design already had natural joints (snapshot filter,
  corpus walk, evaluator load, write route). The risk is recorded in
  [Risks and Open Questions](./risks.md).
- **Generic names carry less context in dashboards.** `HuntId=<uuid>` on a finding says
  less than `YaraHuntId` did; mitigated by per-engine *event kind* names surviving
  (`YaraHuntChunk` queues and `YaraHunter` workers stay engine-named where operators
  watch depths and stats).
- **An engine enum with one variant at launch.** Users see an `engine` field with exactly
  one legal value until the sigma engine ships; the create route 400s the rest. Honest,
  if slightly awkward.
- **Chassis counters are lowest-common-denominator.** `skipped` means "the engine's
  admission policy declined it" (yara: oversize) — engines with richer taxonomies
  surface detail in `error_samples` and their own stats, not in new chassis fields.

The alternative — shipping `YaraHunt` and renaming after implementation — would have paid
the same renames against live code, archived backups (append-only enums), wire formats,
and a shipped UI. Paper-only was the one moment this change was cheap.
