# Possible Today

This page grades each of the [eight questions](./telemetry.md) against the session store as
ThorPal writes it today — chiefly `transcript.jsonl`, `session.json`, and `forum.json` under
`~/.thorium/sessions/<NAME>` (see [Sessions](../ai.md#sessions)) — and then specifies the
bundle format and export pipeline the answerable ones justify. The approach is sound: a
census over stored transcripts alone can count tool calls and failures, find repeat-failure
loop sessions, and cross-check token ledgers — all without touching message text, producing
kilobytes of aggregates per session from megabytes of transcript.

> **Implemented.** The export pipeline below ships as `thorctl analyze telemetry export`,
> riding the read-only scanner behind `thorctl analyze usage` (one shared streaming pass
> serves both commands); `explore` ships alongside it over bundles or the local store.

## Verdicts at a Glance
---

| Question | Verdict | Primary sources |
|----------|---------|-----------------|
| 1. How many tokens were used | **Answerable** | `usage` transcript events; per-agent ledger in `session.json` |
| 2. How were tokens used | **Partial** | per-agent `usage` events; replayed context-composition estimates |
| 3. Which tools were called most | **Answerable** | `tool_call` / `tool_result` events |
| 4. Which tools were never called | **Answerable** (current builds) | per-role `tools_offered` manifest lines |
| 5. Which tools are useful | **Proxy only** — [Future Work](./future_work.md) | success and reuse rates, promotions |
| 6. Which tools broke down | **Answerable** | `tool_result` failures, normalized signatures |
| 7. What decision points when stuck | **Mostly answerable** | ask/termination/pressure events; forum end-state |
| 8. Were detector leads useful or false positives | **Answerable (heuristic)** | detector-authored forum posts, comment stances |

## Question by Question
---

### 1. How many tokens were used

Every model turn appends a durable `usage` event to `transcript.jsonl` — the agent that spent
the tokens plus `input`, `output`, `cached`, `cache_creation`, and `total` counts, timestamped.
`session.json` carries the cross-check: a cumulative per-agent ledger whose JSON keys are
`input_tokens`, `output_tokens`, `total_tokens`, `cached_input_tokens`,
`cache_creation_input_tokens`, `tool_use_prompt_tokens`, and `reasoning_tokens`. Summation is
exactly what [`thorctl analyze usage`](../ai.md#the-usage-tui) already does offline. Two
caveats: the reasoning and tool-use-prompt buckets exist only in the cumulative ledger (the
per-turn events do not carry them), and the handful of turns the model layer suppresses emit
no `usage` event — a fatal unknown-tool turn is missing from the per-turn timeline and the
cumulative ledger alike, while a repaired invalid-call turn inside a completed run is counted
in the ledger but not the timeline, so the two can disagree slightly in either direction.
Bundles ship raw token counts only; dollars are computed on the
receiving end from a pricing table, the same split the session store already uses.

### 2. How were tokens used

Per agent and over time: `usage` events are keyed by agent and timestamped, so per-role
totals, busy windows, and hour-bucketed burn rates all fall out of the transcript — the usage
TUI computes these today. By content category: replaying the stored events reconstructs the
chat TUI's context-spend breakdown — estimated tokens in tool results, tool arguments,
thinking, forum posts and comments, responses, and user text, plus estimated context tokens
per tool.
Honest caveats: the breakdown is **session-wide** (per-agent composition is not tracked), and
it is an estimate derived from stored text sizes, not provider-reported truth. A bundle ships
the estimated numbers, never the text they were estimated from.

### 3. Which tools were called most

Each call writes two events: `tool_call` (agent, call id, tool name, arguments) and
`tool_result` (the same call id, the result, and a success flag). Joining on the call id
yields per-tool and per-agent call counts, failure rates, and wall-clock durations from the
timestamp deltas (older sessions may predate per-event timestamps, so duration math degrades
gracefully to counts). The usage TUI already maintains per-session and store-wide per-tool
call counts; telemetry extends that counting pass with the success and duration dimensions.

### 4. Which tools were never called

Answerable on sessions written by current builds: at build time the harness appends a
`tools_offered` transcript line per role (plus `main`) carrying the sorted model-visible
roster that role's agents were actually offered — always-on orchestration tools, registry
tools after group expansion, MCP-advertised names, and delegate tools alike — plus the
thorpal version that resolved it. "Never called" is then an exact set difference against the
`tool_call` events, per role or session-wide; a resumed session appends a fresh set per
process run, so consumers union per role (and roster drift across binary upgrades stays
visible). Sessions recorded before the manifest shipped cannot answer this retroactively —
the roster was resolved at run time and never written down (see
[Future Work](./future_work.md#tools-offered-manifest)); for those, a bundle can only report
"called zero times among tools other sessions called", a different and weaker claim.

### 5. Which tools are useful

Only by proxy today: success rate per tool, repeat use within and across sessions, and
promotions — a Rhai script kept in the [script library](../scripting.md) or a saved
[memory](../memories.md) (both store entries that record their origin session) is an
implicit vote for the tools that produced it. Detector-question outcomes (question 8) add
one more proxy. Two things are missing: any explicit signal — an analyst accepting a report
section, a forum answer confirmed correct — and an outcome denominator, since how much of
the binary was actually examined is tracked only in the tracing log today. Both are
specified in [Future Work](./future_work.md).

### 6. Which tools broke down

At level 0: `tool_result` events with `success: false`, bucketed per tool — failure counts
and rates with zero content risk. At level 1, bundles add **normalized failure signatures**,
bounded multi-line prefixes of failing results. The recipe is normative (receivers cluster
on exact string equality) and versioned by the bundle's `signature_recipe` stamp; recipe 2,
in order:

1. Take the first 4,096 characters (never splitting a code point).
2. Split into lines on `\r\n`, `\r`, or `\n`.
3. For each line, in order: flatten whitespace runs to one space and trim; skip the line if
   it is now empty; collapse absolute path tokens to `PATH` (a token starting `/` with a
   second segment, `~/`, a drive prefix, or `\\` — begun at the line start, after
   whitespace, or after an opening `` ` ``, `'`, `"`, `(`, `[`, `<`, or `=`, and ending
   before whitespace or a closing `` ` ``, `'`, `"`, `)`, `]`, or `>`, so quoted paths
   collapse with their delimiters kept); collapse `0x`-hex runs, then bare hex runs of
   four-plus characters carrying a digit, then remaining decimal runs, each to `N`; defang;
   cap the line at 160 characters.
4. Drop a line identical to the previous kept line.
5. Keep at most 16 lines joined by newlines, stopping before a line that would push the
   total past 1,500 characters — whole lines only; the first line always fits.

Bundles without a stamp carry recipe 1 (the original single-line, 90-character form);
signatures cluster only within one recipe, so mixed fleets cluster per recipe. Loop
detection rides on top: three or more consecutive identical failure signatures from one
agent on one tool flags a stuck tool loop — under multi-line signatures a loop whose error
tails vary from call to call no longer counts, by design.
What this cannot yet say is *why* a tool failed — bad arguments vs.
environment vs. tool bug — which needs the failure taxonomy in
[Future Work](./future_work.md).

### 7. What decision points did the harness hit when stuck

The transcript durably records the harness's hard moments, each as a countable event:

- `analyst_ask` — the harness stopped to ask a human, with the chosen option and its source:
  `analyst` (a person answered), `analyst_other` (a person wrote in), `auto_default` (nobody
  was there; the recommended option was taken), or `declined`. The auto-default rate is a
  direct measure of decisions made unattended.
- `review_flag` — findings flagged for human review, with a short title (the fuller reason
  lives on the matching `forum.json` record).
- `turn_failed` — a model turn abandoned after the retry budget was exhausted.
- `sub_agent_terminated` — with a death class: `max_turns`, `context`, `cancelled`, or
  `other`, plus whether partial work was salvaged.
- `context_pressure` — warning and critical context-fill events with the token counts.
- `sub_agent_extended` — turn-budget extension events, whose trigger distinguishes
  in-window requests, standing reservations (recorded before any turns are granted),
  reservations converted at the cap, and the one-shot automatic grant.
- `llm_scrubbed` — a content-filter rejection rescued by scrubbing and retrying, linked to
  its retry episode and the killed request's provider id (older lines carry only the agent
  and scrub level).
- `llm_retry` / `llm_retry_resolved` — one line per failed transient attempt (error summary,
  request id, backoff delay) and one per episode close (attempts, outcome), so throttle
  storms and total backoff time count mechanically.
- `provider_paused` / `provider_resumed` — a gate pause's initial publish and its end, the
  resume line carrying the pause kind, duration, failed-probe tally, and whether the provider
  recovered or the operator aborted.
- `report_degraded` — the report generator fell back to a simpler rung.
- `session_resumed` with `interrupted: true` — the previous process died mid-turn
  (resuming a cleanly settled session appends the same barrier with `interrupted: false`).
- `swarm_finished` — per-swarm succeeded/failed member counts.

`forum.json` adds the end-state: questions left `unsolved`, `needs_evidence`, or `blocked`
(blocked is an explicit "ThorPal cannot answer this with its current tools or information"
marker), and
completion-gate tasks that never cleared — both as counts and as per-entry rows
(`forum.entries`: kind, status, author class, detector name-class, technique ids, revision,
engagement, and — at level 1 — the redacted title), capped at 256 rows per session with a
dropped-count remainder. An entry's technique list fills only from the id-bearing template
surfaces below, which seeded questions do not currently carry — so on current builds it is
reserved vocabulary that ships empty. Two title guards: comment-questions have no title, so their rows
ship none at any level (a comment's first body line stands in only in the local explorer —
bodies never ship); and detector-seeded sibling questions carry a harness-appended
`— <file name>` suffix locally, which the fold strips before the title can reach a bundle
(the `sample` index already says which sibling). Together these answer "where did runs stall and
why", provider-side stalls included on current builds — retry storms, backoff time, and
auth/outage pauses persist via the four retry/pause kinds above. Sessions recorded before
those events shipped remain blind to provider stalls (what was once the biggest single gap
here — see [Future Work](./future_work.md#durable-retry-and-pause-events)).

### 8. Were detector leads useful or false positives

[Preseed detectors](../ghidra/detectors.md) seed the forum before the agents start, and the
forum records what happened to each lead. Detector posts are identifiable (they are authored
by the reserved `detector` name), every comment carries a stance toward its parent —
`supports`, `disproves`, or `neutral` — and detector-seeded questions carry a tracked
resolution status. That supports an outcome tally per technique id: **confirmed**
(supporting evidence landed), **disputed** (an agent posted disproving evidence — the
false-positive signal), **neutral** (engaged without a decisive stance), **ignored** (zero
engagement — the lead cost seeding tokens and bought nothing), and for questions, solved vs.
blocked. Each technique's tally also carries a lead count and a confidence histogram, and
bundles ship up to 128 per-lead rows per session (`detector_leads`: technique, post class,
shape, confidence, suspicion, site count, artifact *class*, verdict, engagement, and — on
sessions with structured provenance — the source and native detection name, marked `exact`).
Sessions recorded by current builds additionally carry the seeding roster (`detectors_run`),
so *detectors that never fired* is an exact per-session answer. Technique ids come from the tool
side — native ATT&CK constants plus ids minted by capa rules, YARA matches, and library
detector scripts — never from sample content: the fold reads ids only from the template
surfaces that carry them (bucket posts' `Techniques:` lists, rollup posts' per-site lines,
finding comments' opening `[<id> …]` line), so question and annotation bodies — which embed
sample-derived text such as archive entry names — are never parsed for ids. The vocabulary
is still not a closed enum, so non-native ids get the same export scrutiny as level-1
strings. Two further caveats:
stances are per-comment evidence relations, so the verdict is a heuristic rule, not
a recorded judgment; and on sessions recorded before structured provenance shipped, the
technique/confidence of each post is parsed from detector-authored template text (current
builds persist those facts structurally — see
[Future Work](./future_work.md#structured-detector-provenance)). The disposition half still
hardens in [Future Work](./future_work.md).

## The Bundle
---

One JSON document (optionally gzipped), human-readable so the preview *is* the format:

- **Level 0 (default)** — strictly content-free. Counts, token totals, tool-name histograms,
  failure rates, durations, enum tallies, timestamps, and hashes. The only strings are tool
  names, event kinds, enum values, agent role names, model and tool-version ids, technique
  ids, timestamps, and hex digests.
- **Level 1 (opt-in)** — level 0 plus the normalized failure signatures described under
  question 6.

Illustrative sketch — not normative; the `bundle_version` field is the compatibility contract:

```json
{
  "bundle_version": 1,
  "signature_recipe": 2,
  "level": 1,
  "created_at": "2026-10-28T17:04:00Z",
  "tool_version": "<thorctl version>",
  "sessions": [
    {
      "session_id": "9f2c…e1a7",
      "binary_sha256": "d4b1…09c3",
      "anonymized": true,
      "model": "<model-id from the saved session>",
      "duration_ms": 5407000,
      "totals": {
        "input_tokens": 543826, "output_tokens": 410552,
        "cached_input_tokens": 21083190, "cache_creation_input_tokens": 2991217,
        "tool_use_prompt_tokens": 310882, "reasoning_tokens": 88241,
        "total_tokens": 25028785
      },
      "agents": { "ReverseEngineer": { "instances": 9, "total_tokens": 15206001 } },
      "tools": { "run_script": { "calls": 212, "fails": 17, "p50_ms": 1410 } },
      "decision_points": {
        "asks": { "auto_default": 4, "analyst": 1 },
        "terminations": { "max_turns": 2, "context": 1 },
        "context_pressure": { "warn": 6, "critical": 1 },
        "turn_failed": 0, "llm_scrubbed": 1, "interrupted_resumes": 0
      },
      "forum": { "posts": 61, "unsolved": 3, "blocked": 1, "tasks_unmet": 2,
        "entries": [
          { "kind": "question", "status": "blocked", "author": "detector",
            "name_class": "q-unpack", "techniques": ["T1027"], "updated": 2,
            "untouched": false, "tasks": 2, "tasks_unmet": 2,
            "title": "Can the payload be unpacked?" }
        ] },
      "detectors": { "T1027": { "confirmed": 2, "disputed": 1, "ignored": 3,
        "leads": 6, "confidence": { "likely": 4, "unsure": 2 } } },
      "detector_leads": [
        { "technique": "T1027", "post_class": "fn", "shape": "finding",
          "confidence": "likely", "suspicion": 60, "sites": 3,
          "verdict": "confirmed", "exact": true }
      ],
      "detectors_run": { "packing": { "technique": "T1027.002", "runs": 1, "hits": 4 } },
      "signatures": [
        { "tool": "run_script",
          "sig": "script error: line N: unknown fn 'N'\ncalled from PATH\n  at frame N",
          "count": 11, "loop": true }
      ]
    }
  ]
}
```

Session names can be descriptive (an analyst may name a session after a case), so bundles
carry `session_id = sha256(name)` instead; the binary is identified by its SHA-256 exactly as
[anonymize-at-rest](../ai.md) already does.

## Export Pipeline
---

1. **Scan** a sessions root exactly like `thorctl analyze usage`: strictly read-only,
   tolerant of live writers and torn tails, honoring `--sessions-root` so a copied tree works.
2. **Fold** each session once — a single streaming pass that reduces the transcript to
   counters: the usage scanner's existing pass, extended with success, duration, and
   decision-point counters, plus a new pass over `forum.json` for the end-state and
   detector outcomes.
3. **Normalize** (level 1 only): run the signature recipe over failing tool results (see
   question 6's normative recipe — line-structured, number-scrubbed, path-collapsed,
   defanged, capped at 16 lines / 1,500 characters).
4. **Redact**: apply the run's deny tokens (`--redact-tokens`, the same file the chat flag
   takes) and the library's neutral-alias scheme to every level-1 string; deny lists are
   never persisted, matching the session store's posture.
5. **Preview and confirm**: render the exact bytes that will be written and require
   confirmation (the [Telemetry TUI](./tui.md) specifies the flow), then write the bundle.

The command loads config best-effort, like `usage` and `batch` do, so it runs on a box with
no thorctl config at all.

## What Never Leaves the Machine
---

The denylist is structural — these are never read into the bundle, at any level:

- Chat and agent message bodies — `chat.json` (the main conversation), `agents/*.json`
  (each sub-agent's full transcript), and `btw/` (side conversations)
- Tool arguments and tool results (only names, counts, durations, and — at level 1 —
  normalized failure signatures derived from failing results)
- Forum post and comment bodies, analyst questions, and justifications (counts, stances,
  statuses, technique ids, and per-entry rows of closed-vocabulary classes always; question
  titles and task descriptions are the one exception — level 1 only, redacted, and
  individually droppable on the review screen)
- The report (`report.md`), session scripts (`scripts/`), and carved files (`extracted/`)
- File names and paths — samples are identified by SHA-256 only
- Deny-token lists themselves

API keys never reach the bundle because they never reach the store: the saved session keeps
a sanitized provider config, and secrets are re-read from live config on resume (see
[Sessions](../ai.md#sessions)).

## CLI Sketch
---

```bash
$ thorctl analyze telemetry export                          # all sessions, level 0, preview then write
$ thorctl analyze telemetry export --level 1                # opt into normalized failure signatures
$ thorctl analyze telemetry export --session NAME --out b.json
$ thorctl analyze telemetry export --sessions-root ./copy   # read-only over a copied store
$ thorctl analyze telemetry export --redact-tokens deny.txt --yes   # headless: skip preview, stable stderr tail
```

The interactive flow — session picker, tier choice, preview, redaction review — is specified
in [Telemetry TUI](./tui.md); the gaps this page defers are specified in
[Future Work](./future_work.md).
