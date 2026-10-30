# Telemetry TUI

> **Implemented.** Both verbs below ship.

A new `thorctl analyze telemetry` subcommand — a sibling of `chat`, `usage`, and `batch` —
hosts both halves of the feature: the analyst-side **export** flow and an **explorer** for
investigating telemetry, whether that is a local session store or a stack of bundles mailed
in from the field. [`thorctl analyze usage`](../ai.md#the-usage-tui) stays what it is today:
local cost tracking. Like `usage`, `telemetry` loads config best-effort (it must run on a box
with no thorctl config) and is strictly read-only over session stores.

## The Subcommand
---

```bash
$ thorctl analyze telemetry export [--level N] [--session NAME] [--sessions-root DIR] \
                                   [--redact-tokens FILE] [--out FILE] [--yes]
$ thorctl analyze telemetry explore [BUNDLE...]             # no args: explore the local store
```

`export` drives the flow below and writes a bundle file; `explore` opens one or more bundle
files — or, with no arguments, the local sessions root, computing the same aggregates on the
fly without writing anything.

## Export Flow
---

Five screens, in order:

1. **Session picker** — a multi-select list over the scanned sessions root (the same
   background scanner the usage TUI runs), showing name, age, size, and token totals.
2. **Tier choice** — level 0 or level 1 (defined under
   [Tiered Bundles](./telemetry.md#tiered-bundles)), with a plain-language diff of exactly
   what level 1 adds (normalized failure signatures and the forum end-state's titles) and
   what it still excludes, plus both tiers' exact byte sizes for the current selection and
   the level-1 delta (bytes, signatures, titles).
3. **Redaction review** — deny tokens (`--redact-tokens`) and neutral aliases are applied
   first; this screen lists every level-1 signature next to its tool name and every level-1
   forum title with its entry count, so the analyst can drop any individual signature or
   title, or fall back to level 0 entirely.
4. **Preview** — the exact bytes that will be written, rendered in full after redaction and
   any drops. Nothing shown is withheld and nothing unshown ships: the preview *is* the
   bundle, and changing anything on the review screen returns here for a fresh render —
   confirmation always happens on the final bytes. `t` toggles the tier in place (`0`/`1`
   set it directly) and the header shows the exact byte delta between the tiers; drops
   survive the toggle, and switching up to level 1 warns until its signatures and titles
   have been seen on the review screen (`Esc` goes there).
5. **Write** — the bundle path and a one-line summary (sessions, level, byte size).

Headless parity: `export --yes` skips the TUI, applies the same redaction, and prints a
stable, machine-parseable stderr tail — the same contract the headless analyze run already
honors for its `usage:` summary line, which batch tooling parses today. The line (pinned by a
unit test) is:

```text
telemetry: sessions=<N> level=<L> signatures=<S> bytes=<B> sha256=<hex of the written bytes> out=<PATH>
```

The digest covers the exact bytes written (before any gzip framing), so an analyst can
certify what left the machine; `--session NAME` repeats per session for headless parity with
the picker's multi-select.

## Explore Flow
---

The explorer cycles through screens the way [the usage TUI](../ai.md#the-usage-tui) cycles
its seven, over whichever source is open (local store or merged bundles):

- **Fleet overview** — sessions, total tokens, estimated cost, failure and interruption
  rates, split by harness version and model when they disagree (the per-session manifest
  version outranks the exporting tool's); the fleet's sessions list underneath, and over a
  local store Enter drills into one.
- **Tool league table** — calls, failure rate, median duration per tool; sortable. An
  "offered, never called" column lights up for sessions whose stores carry the
  [tools-offered manifest](./future_work.md#tools-offered-manifest) (current builds write it).
- **Failure signatures** — level-1 clusters ranked by count, loop flags highlighted;
  Enter opens the full multi-line signature with its session spread and recipes.
- **Stuck points** — asks by source (the auto-default rate front and center), termination
  classes, context-pressure counts, scrubs, degraded reports, interrupted resumes, and the
  forum end-state as four explorable rows (unsolved / needs evidence / blocked / tasks
  unmet) whose Enter lists the matching entries per session, titles per the level-1 posture.
- **Detector outcomes** — per-technique lead counts, a fact/likely/unsure/untrusted
  confidence histogram, and the confirmed / disputed / neutral / ignored tallies (see
  [question 8](./possible_today.md#8-were-detector-leads-useful-or-false-positives));
  Enter opens the technique's per-lead pop-out (post titles local-only), and a
  "never fired" section lists rostered detections with zero hits fleet-wide.
- **Token economics** — cache savings, per-agent-role token shares, hour-bucketed burn.

Dollar figures are always computed at render time from the local pricing table — bundles and
the session store both carry raw token counts, so a pricing correction retroactively fixes
every view.

## Reused vs New
---

| Reused or extended | New |
|--------------------|-----|
| The shared terminal setup/teardown layer every analyze TUI uses (built so a second TUI cannot drift from the chat TUI's terminal handling) | Bundle schema, writer, and reader |
| The usage TUI's read-only background scanner and its per-session counter pass | A `forum.json` pass (end-state and detector outcomes) |
| The usage TUI's screen set as the explorer's base | Signature normalizer (specified under [question 6](./possible_today.md#6-which-tools-broke-down)) |
| Session drill-down replay — agent stats and gantt views over a finished session | Preview and redaction-review screens |
| The presentation-side pricing table | Multi-bundle merge |

One implementation note: the usage TUI's aggregate types are view-model structs today — the
bundle needs serializable twins of them (plain data-transfer types), which is also where the
`bundle_version` contract lives. Drill-down replay remains available only when exploring a
local sessions root: bundles carry no messages to replay, by design.

## Layout and Keys
---

The explorer composes like the chat TUI: a numbered screen bar on top, the active screen in
the main pane, and — on terminals 110 columns and wider — a right sidebar summarizing the
source (bundle files, levels, exporter versions, and warnings such as mixed signature
recipes or bundles predating per-entry rows), the filtered fleet's quick stats, the active
version filter, and the screen's key legend. Details open as centered pop-out windows over
the screen: `Enter` opens the screen's context pop-out (on the fleet over a local store it
drills into the session instead; `i` opens the bundle-safe session detail in both modes),
`Esc` closes the top-most layer — pop-out, then palette, then the app. `Space` opens the
shortcut palette; `?` (or `Space` then `h`) opens the per-screen help. `f` cycles a
harness-version filter over every screen's aggregates, and `r` rescans a local store.

Every screen in both TUIs carries a help page explaining what it shows, where the numbers
come from, and what each column means — including the `loop!` flag: three or more
consecutive identical failure signatures by one agent on one tool, the stuck-tool-loop
marker. A unit test pins every rendered column to an explanation on its screen's help page.
The full key map:

| Keys | Explorer | Export wizard |
|------|----------|---------------|
| `1`-`6` · `Tab`/`Shift-Tab` | jump to / cycle screens | — |
| `↑`/`↓` (`k`/`j`) · `PgUp`/`PgDn` | move the cursor · scroll | move · scroll |
| `Enter` | open the cursor's pop-out (fleet drills into a local session) | continue; on the preview, write the bundle |
| `i` | session-detail pop-out (both modes) | — |
| `Space` | shortcut palette | toggle: picker selection, review drop |
| `a` | — | select all / none (picker) |
| `v` / `→` | — | view a review signature in full |
| `t` · `0`/`1` | — | tier toggle on the preview; `0`/`1` (or `←`/`→`) choose on the tier screen, `0` falls back on review |
| `f` / `r` | cycle the version filter / rescan a local store | — / rescan (picker) |
| `?` (or `Space`, `h`) | per-screen help | per-screen help (`?` or `h`) |
| `Esc` | close the top layer: pop-out, palette, then quit | close the pop-out, else back one screen |
| `q` · `Ctrl-C` | quit | quit |

## Personas
---

**The analyst (field)** uses `export`: picks sessions, previews, prunes signatures, ships the
file through whatever channel their program allows. The preview and deny-token screens exist
so they can certify what left their machine. Most analysts never open the explorer.

**The Thorium admin (receiving end)** uses `explore` over many bundles: merges a fleet,
compares releases and models, reads the tool league table and stuck-point screens, and files
the [Future Work](./future_work.md) items the data motivates. When something looks wrong in
one bundle, the matching conversation stays with the analyst — the admin asks, the analyst
drills into their local store with the same explorer.
