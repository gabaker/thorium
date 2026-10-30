# Future Work

The gaps in the [feasibility verdicts](./possible_today.md) share one cause: data the harness
computes in memory but never persists. Each item below either upgrades a *Partial* or
*Not today* verdict to *Answerable*, or hardens an answer that page can give only by proxy or
heuristic; most are small transcript-durability changes, and the last lives on the receiving
side. Bundles carry a `bundle_version`, so these can land incrementally — receivers simply
get richer bundles from newer sessions.

## At a Glance
---

| Item | Unlocks | Effort | Value |
|------|---------|--------|-------|
| [Durable retry and pause events](#durable-retry-and-pause-events) | Q7 | **Done** | High |
| [Tools-offered manifest](#tools-offered-manifest) | Q4 | **Done** | High |
| [Tool-failure taxonomy](#tool-failure-taxonomy) | Q6 | Medium | High |
| [Per-request detail on usage events](#per-request-detail-on-usage-events) | Q2, fleet splits | Low–medium | Medium |
| [Per-turn reasoning/tool-use split](#per-turn-reasoningtool-use-split) | Q1, Q2 | Low | Medium |
| [Scrub-to-retry linkage](#scrub-to-retry-linkage) | Q7 | **Done** | Low–medium |
| [Coverage persistence](#coverage-persistence) | Q5 | Low | Medium |
| [Explicit usefulness signals](#explicit-usefulness-signals) | Q5 | Medium | Medium |
| [Structured detector provenance](#structured-detector-provenance) | Q8 | **Partial** | Medium–high |
| [Cross-bundle aggregation](#cross-bundle-aggregation) | all, fleet-wide | Medium–high | High |

## Durable Retry and Pause Events
---

> **Implemented.** The retry and pause lifecycle persists to the transcript instead of being
> dropped at the live/durable boundary.

Each failed transient attempt appends an `llm_retry` line (attempt counter, error summary,
provider request id, backoff delay), and each episode closes with an `llm_retry_resolved`
line (attempts consumed, outcome). A gate pause persists as a pair: `provider_paused` on the
initial publish (pause kind, error, request id) and `provider_resumed` at the end, carrying
the whole episode's cost — `paused_ms`, the failed-probe tally, and whether the provider
recovered or the operator aborted. Probe re-emissions stay live-only banner refreshes (their
tally lands on the resume line), so a pause costs two lines however long it runs; retry lines
are bounded by the per-request attempt budget, so volume never justified the counter-checkpoint
fallback this item once sketched. This closes the biggest hole in question 7: *how often did
the harness stall on the provider rather than the analysis*. Bundles exported from sessions
that predate these events are permanently incomplete for provider-stall telemetry — which is
why it landed first.

## Tools-Offered Manifest
---

> **Implemented.** Each built role's resolved tool roster is written into the session store.

The store records the workflow's configured tool and group names, but groups expand against
the registry compiled into the running binary, and [MCP servers](../tools.md#mcp-tools)
advertise their tools only at connect time — so the offered roster could not be reconstructed
afterward. Every build now appends one `tools_offered` transcript line per role (plus
`"main"`), taken from the built agent itself: the sorted model-visible roster — always-on
orchestration tools, registry tools after group expansion, MCP-prefixed names, and delegate
tools alike — plus the thorpal version that resolved it, at a few hundred bytes per role.
Every instance of a role shares the one built agent, so the per-role line is the per-agent
truth the original sketch asked for; a resumed session appends a fresh set per process run,
keeping roster drift across binary upgrades visible (consumers union per role). "Never
called" is now an exact set difference. Like the item above, sessions recorded without
manifest lines can never answer question 4 retroactively.

## Tool-Failure Taxonomy
---

> **Not implemented.** Classify failing tool results instead of only counting them.

`success: false` plus a level-1 signature says *what* broke, not *why*. A small taxonomy —
invalid arguments, environment/bridge down, timeout, output overflow, tool bug — attached at
emit time (where the tool knows the cause) turns breakdown telemetry into a routing signal:
argument failures point at tool descriptions, environment failures at the deployment, tool
bugs at the backlog. Signature rules shipped with the exporter can approximate this for old
sessions, but emit-time classification is the durable fix.

## Per-Request Detail on Usage Events
---

> **Not implemented.** Carry the model id, finish reason, and latency on each `usage` event.

Today a `usage` event ties tokens to an agent but not to a model (one lives in `session.json`
per session), a finish reason, or a latency. The provider layer already has all three in hand
when the event is emitted; persisting them enables per-model comparison across a fleet of
bundles and tail-latency telemetry. (The assistant message id already survives in the saved
histories; the provider request id and finish reason are what is genuinely dropped today.)

## Per-Turn Reasoning/Tool-Use Split
---

> **Not implemented.** Emit the reasoning and tool-use-prompt token buckets on each `usage`
> event, not only in the cumulative ledger.

The per-agent ledger in `session.json` tracks `reasoning_tokens` and
`tool_use_prompt_tokens`, but per-turn events carry only the input/output/cache buckets — so
"how much did thinking cost *over time*" is currently an end-of-run number. This closes
question 1's per-turn caveat and makes the thinking and tool-use share of question 2
provider-exact over time; the content-category breakdown remains an estimate.

## Scrub-to-Retry Linkage
---

> **Implemented** (pulled forward alongside the durable retry events — it edits the same
> live/durable seam).

The durable `llm_scrubbed` marker now carries the `retry_id` of the episode that triggered it
plus the killed request's provider id, the durable retry lines carry the same episode key, and
`turn_failed` keeps its structured request id instead of only embedding it in the error text —
so bundles can report "scrubbing rescued N requests, at a cost of M extra attempts" by joining
`llm_scrubbed` to its episode's `llm_retry_resolved`.

## Coverage Persistence
---

> **Not implemented.** Persist an end-of-run snapshot of the function-coverage ledger.

The harness tracks which binary functions each agent examined, but the summary goes only to
the tracing log. A counts-only snapshot in the store (functions seen / decompiled / examined,
per agent) gives usefulness telemetry a denominator: a tool with modest call counts that
drives most of the coverage is earning its keep.

## Explicit Usefulness Signals
---

> **Not implemented.** First-class events for outcomes, replacing question 5's proxies.

Candidates, roughly in order of signal quality: analyst verdicts on
[review flags](../forum/how_it_works.md) and report sections (accepted / corrected / rejected),
forum answers later confirmed or overturned, and script/memory promotions recorded as events
with the tools that produced the promoted artifact. Each is an enum plus ids — content-free
by construction.

## Structured Detector Provenance
---

> **Partially implemented.** Each seeded node's detector identity now persists as structured
> data; the first-class false-positive disposition remains future work.

Detector findings internally carry a technique id, confidence, suspicion score, and source
(native detection, capa import, YARA, library script, emulation) — seeding used to flatten
the first three into post text and drop the source outright. Current builds persist a
structured provenance record per seeded node in `forum.json` (`detector_provenance`,
deliberately outside every model-visible forum view, so it costs zero context tokens):
technique, confidence, suspicion, source, the producing detection's name, site count, the
artifact's *class* (never its value), and whether the finding was suppressed into the noise
rollup; seeded questions record what derived them. Each seeding pass also appends a
`detectors_run` transcript line naming every registry detection that ran -- zero-hit and
failed detections included -- so *detectors that never fire* is answerable per session
against the registry that actually ran, not a later reader's. Sessions recorded before this
parse templates as before, with the parsed era's inherent caveats: a hostile comment body
can forge the `Site:`/`Sites (n):` facts its lead row reports (enums and numbers only — the
fold prefers the last match, which beats text embedded before the template's own lines, but
a recovered artifact value is quoted after them, so sample content there can still win;
provenance outranks parsing wherever it exists), and rollup leads parsed from the body see
only the lines the rollup's display cap kept. Still future: an explicit "mark false positive" disposition for
agents, which would turn the stance heuristic into a recorded judgment.

## Cross-Bundle Aggregation
---

> **Not implemented.** Aggregate fleets continuously, beyond the explorer's file-level merge.

Everything above describes one analyst's export, and merging a handful of bundle files is
already part of the [Telemetry TUI](./tui.md) design. This item is the step beyond it:
fleet-scale aggregation — same-schema bundles summed across releases and models so
regressions show up as deltas rather than anecdotes — and, later, a Thorium API ingestion
route so bundles can be uploaded instead of hand-carried.

## Sequencing
---

Land the two cheap, lossy-if-missing items first — durable retry/pause events and the
tools-offered manifest — because every bundle exported before them is permanently incomplete
for questions 4 and 7. Both have landed (the
retry/pause events with the scrub-to-retry linkage pulled forward, since it edits the same
seam). The taxonomy and
usefulness signals can trail: level-1 signatures already cluster failures well enough to
steer development. Aggregation is independent and lives entirely on the admin side.
