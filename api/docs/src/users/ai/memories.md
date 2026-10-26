# Memories

ThorPal's agents keep a persistent **memory library** of durable, GENERIC
lessons across analysis runs: techniques that worked, workflow rules, verified
family intel, and pitfalls to avoid. Memories are deliberately not a place for
sample- or session-specific facts — hashes, paths, decoded strings, and session
details belong on the per-session [forum](./forum/forum.md), and the write side
refuses them. The library is owned by the **Researcher** sub-agent in the
builtin workflow: every agent reads memories, but only the researcher saves
them, so the library stays a curated set of lessons rather than a dumping
ground.

## Why

The forum is the right home for everything a run learns about *its* sample,
but it dies with the session — the only knowledge that used to survive a run
was the Developer's [script library](./scripting.md). Evaluation runs kept
hand-writing "memory candidate" lessons (a technique that worked, a workflow
rule, a family trait verified against a binary) that had nowhere to go except
a human editing agent preambles. The memory library automates that loop, and
gives the researcher — whose whole value is knowledge — something durable to
curate and recall from instead of answering purely from training recall.

## The Memory Library
---

Memories live under `~/.thorium/memories/` by default, overridable with the
`ai` config's `memories_dir` key. Each memory is a single self-contained
markdown file whose leading `---`-fenced front-matter header carries its
metadata:

```markdown
---
name: attribution-discipline
kind: pitfall
tags: attribution, citation-hygiene
promoted-from: mwater-triage (2026-10-21)
description: Never promote recalled family lore to a conclusion; post it as a
  falsifiable hypothesis for the reverse engineer to verify.
---

The lesson body in plain markdown...
```

- `kind` is one of `technique` (how to decode/resolve/extract something),
  `workflow` (how to run an analysis well), `family-intel` (verified knowledge
  about a malware family), or `pitfall` (a mistake future runs should not
  repeat).
- `tags` and `description` are what future runs search to decide whether a
  memory applies — an undocumented memory can never be found.
- `promoted-from` records the saving session and date; it renders as a neutral
  `session-<8 hex>` id on every model-visible read, exactly like the script
  library's provenance.

Writes are atomic (a `.tmp` sibling renamed over the target), concurrent
sessions race at last-writer-wins granularity per memory, and parsing is
lenient — a hand-dropped markdown file with no header still lists and loads.

A small set of **builtin memories** ships compiled into the binary (currently
`resolve_aarch64_adrp_add_pairs`, the ADRP/ADD resolution guide that used to
be frozen into the researcher's preamble) and merges read-only into every
listing; saving a memory under a builtin's name shadows it, like session
scripts shadow library scripts.

## The memory Tools
---

| Tool | What it does |
|------|--------------|
| `list_memories` | List every memory (builtins included) with its name, kind, and description |
| `search_memories` | Search every memory (headers included) for a case-insensitive substring |
| `read_memory` | Read a memory's full source by name |
| `save_memory` | Save a durable generic lesson (researcher-only in the builtin workflow) |

Memory-enabled agents also get a **memory index** — one line per memory, capped
at 25 with a pointer at `list_memories` — injected into their spawn prompts
next to the forum context, rendered fresh per spawn so memories saved earlier
in the same run show up for later delegations. Their preambles carry a
generated recall-discipline section: consult the index before re-deriving a
technique or asserting facts from recall, cite memories by name, and treat
memories as prior-run knowledge that must be verified against *this* sample
before it becomes a conclusion.

`save_memory` validates before writing: a non-empty description and body
(capped at 16KB — distill the lesson, don't archive raw output; a body that is
nothing but a stale header strips to empty and is refused), at most 8 tags
with no control characters (a newline in a tag could splice extra lines into
the rendered header, so it is refused and the rendered header is additionally
re-parsed to prove it round-trips before anything lands on disk), and a
**generic-content guard** that refuses 32+ character hex runs containing a
letter (sample hashes/raw IOCs; long decimal constants stay fine) and any
mention of this run's sample names or session name. Name
technique/workflow/pitfall memories by TECHNIQUE, never by family —
family-named memories render as opaque neutral ids in blinded runs. A
`family-intel` memory is the one exception: it should carry the family's name
and aliases in its name and tags, so family-blinded evaluations can withhold
exactly that family's intel (see below) — and because a sample or session is
routinely named after the family, family intel is exempt from the
sample-/session-name rule (the hex-run rule still applies). Saving over an
existing name is refused unless `overwrite: true`.

The `finish` tool nudges the main agent exactly once per run — after the
decoded-constant, rename, and script-promotion nudges — to review the run for
durable generic lessons and delegate saving them to the researcher.

## Blinding and `--blind-family`
---

Memories are a cross-run surface, so they get the same render-time blinding
treatment as the script library (carried-over library content can leak family
names into blinded runs, so memories follow the same rules from day one):

- **Provenance always neutralizes**: `promoted-from:` renders as
  `session-<8 hex of sha256(name)>` on every model-visible read (the `builtin`
  provenance is exempt — it names no session). On-disk files stay pristine
  operator-side truth.
- **Under `--anonymize`** every memory name renders as a neutral
  `mem-<8 hex of sha256(name)>` id (resolvable back through the tools), and
  the `--redact-tokens` deny list scrubs family tokens out of memory text with
  stable `fam-<4 hex>` placeholders, exactly like library scripts.
- **`--blind-family <NAME>`** (repeat the flag for each alias) hides matching
  `family-intel` memories *entirely*: a family-intel memory whose name or
  content matches any blind token is treated as nonexistent by every surface
  (listing, search, read, and the spawn index), because family intel's content
  can reveal a blinded evaluation's ground truth even after token scrubbing.
  Other families' intel and all generic kinds stay visible (render-blinded as
  usual), and the blind tokens also join the deny-token scrub set. Like
  `--redact-tokens`, the list is eval ground truth: the model never sees it,
  it is never persisted into the session store, and a resume must re-supply
  it.

  Two deliberate consequences of "hidden means nonexistent on *every*
  surface": the run's **own saves never hide from the run that wrote them** (a
  save that immediately vanished from reads would itself be an oracle on the
  blind list; the exemption is per-session, so a resume hides them again), and
  a save colliding with a hidden entry **overwrites it silently** rather than
  refusing (an "already exists" error on a name every read reports missing
  would be an existence oracle — last-writer-wins is the library's contract,
  and blinded evaluations should use an isolated `memories_dir` anyway). Note
  also that hiding keys off the parsed `kind:` header — a hand-dropped intel
  file without one parses as `technique` and is never hidden, so set
  `kind: family-intel` explicitly when dropping intel in by hand.

## Pooled Evaluations
---

`thorctl analyze batch` children share the one `--config`, so pooled runs share one
memories directory with last-writer-wins races per file — the same contract as
the script library. That sharing is the cross-run learning loop working as
intended, but for a *blinded* evaluation it also means later units can read
intel written by earlier units of the same corpus. Point `ai.memories_dir` at
an evaluation-local directory for full isolation, and/or pass each unit's
family aliases via `--blind-family` (a per-unit batch integration that fills
`--blind-family` from the manifest's family names is a natural follow-up).
