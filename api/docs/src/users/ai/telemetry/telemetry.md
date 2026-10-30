# Telemetry

This chapter is about telemetry on **ThorPal itself** — token spend, tool health, where runs
get stuck — not the victim or EDR telemetry discussed in analysis write-ups elsewhere in this
book. Analysts run `thorctl analyze` entirely on their own machines, on networks the
ThorPal team can never reach, and every run already persists rich operational evidence in
its [session store](../ai.md#sessions). The plan here: let an analyst
compile a small, privacy-safe **telemetry bundle** from those stores and send it back, so the
ThorPal team can see how the tool behaves across a fleet it can never log into.

> **Implemented.** The store-side durability items shipped first — durable retry/pause
> events, the scrub-to-retry linkage, and the tools-offered manifest (see
> [Future Work](./future_work.md)) — and `thorctl analyze telemetry` ships both halves:
> `export` (the tiered bundle, preview-and-confirm, and headless `--yes`) and `explore`
> (six screens over merged bundles, or the local store computed on the fly).

## The Eight Questions
---

Telemetry earns its keep by answering specific questions, and these are the ones the design
is built around:

1. How many tokens were used?
2. How were the tokens used?
3. Which tools were called most?
4. Which tools were never called?
5. Which tools are useful?
6. Which tools broke down?
7. What decision points did the harness hit when it got stuck?
8. How often was a preseed detector lead useful vs. a false positive?

[Possible Today](./possible_today.md) grades each one against what the session store already
records.

## Why Not Just Ship the Logs
---

Two constraints rule out sending sessions wholesale. **Size:** session stores range from
tens of kilobytes (an aborted start) to over 500 MB, and an archive of many sessions runs
to gigabytes. The slow or
intermittent links available at field deployments cannot move gigabytes per analyst.
**Sensitivity:** the bulk
of those bytes is the analysis itself — decompiled code, recovered strings and IOCs, forum
discussion, report text — exactly the material that must not leave an analyst's machine
wholesale. What travels has to be *aggregates*: counts, totals, enums, and hashes, derived
locally, kilobytes per session.

## Tiered Bundles
---

A bundle is one reviewable JSON file, exported at one of two levels. **Level 0 (default)**
is strictly content-free: token totals, tool-name histograms, failure rates, durations,
decision-point and detector-outcome tallies — the only strings are tool names, event kinds,
enum values, agent role names, model and tool-version ids, technique ids, timestamps, and
hashes. **Level 1 (opt-in)** adds two prose classes, both deny-token-redacted and
individually droppable on the review screen: normalized failure signatures — bounded
multi-line, defanged, number-scrubbed, path-collapsed prefixes of *failing* tool results —
and the forum end-state's question titles and task descriptions, so a receiver can see what
stayed unsolved, not just how much. Post and comment bodies never ship at any level.

## Analyst Trust
---

Nothing ships without review. The export flow renders the exact bytes that will leave the
machine and requires confirmation; deny tokens are applied before the preview; at level 1,
each signature and each forum title can be dropped individually. The session store was already built for this
posture — [anonymize-at-rest](../ai.md#chatting-about-a-binary) identifies blinded
(`--anonymize`) sessions' samples by SHA-256 only, secrets are never written to disk, and
redaction lists are never persisted — and the bundle inherits it.

## In This Chapter
---

| Page | What it covers |
|------|----------------|
| [Possible Today](./possible_today.md) | Question-by-question feasibility against the current session store, the bundle format, and the export pipeline |
| [Future Work](./future_work.md) | Store and harness changes that close the gaps, with effort and value |
| [Telemetry TUI](./tui.md) | The `thorctl analyze telemetry` subcommand: the export flow and the explorer |
