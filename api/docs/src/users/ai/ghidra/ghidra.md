# Ghidra

ThorPal's reverse engineering muscle comes from [Ghidra](https://ghidra-sre.org), driven
headlessly through the `ghidra-cli` bridge. The agents never open the Ghidra GUI; instead a
long-lived bridge runs inside Ghidra's own JVM, so after the first (slow) import and
auto-analysis, every tool call — a decompile, a cross-reference lookup, a memory read —
returns in well under a second.

This section covers how the integration behaves, the
[currently integrated tools](./integrated_tools.md) the agents can call, the
[candidate tools](./candidate_tools.md) ghidra-cli supports that could be added to the
harness, and the [preseed detectors](./detectors.md) that stock the forum with findings and
targeted questions before the agent loop starts.

## How a Session Uses Ghidra
---

- Each binary gets its own Ghidra project under your configured projects directory
  (`~/ghidra-projects` by default), named after the binary's file name without its
  extension.
- The connection is opened lazily on the first Ghidra tool call. A brand-new binary is
  imported and auto-analyzed at that point, with Ghidra's live analysis output streamed
  into the chat log under the running tool call so you can watch its progress.
- An existing project is reused as-is: a binary you have analyzed before (in a previous
  session or in Ghidra itself) skips re-analysis entirely, making repeat sessions fast.
- Analysis results are saved durably to the project as the run goes and flushed on
  shutdown, so a later session — or you, opening the project in the Ghidra GUI — picks up
  everything that was computed.

## Setup
---

The interactive wizard can handle this whole section for you — it detects existing installs
(or downloads the latest release from GitHub), checks for a usable JDK, and sets the
projects directory:

```bash
$ thorctl analyze setup
```

For scripted or manual setup, point Thorctl at a real Ghidra installation (one containing
`support/analyzeHeadless`):

```bash
$ thorctl config --ghidra-install /opt/ghidra
```

or override it per run with `--ghidra-install` / the `GHIDRA_INSTALL` environment variable.
The projects directory can also be changed:

```bash
$ thorctl config --ghidra-projects /data/ghidra-projects
```

Ghidra itself needs a full JDK (not just a JRE) available on the machine, since the bridge
script is compiled inside Ghidra at startup.

## Common Issues
---

- **No install configured** — agents whose tools need Ghidra refuse to start, with an error
  telling you to set `ghidra_install`. Agents without Ghidra tools still work.
- **Hidden path components** — Ghidra 12.1+ rejects any project location whose path contains
  an element starting with `.` (for example `~/.thorium/projects`). Thorctl validates this up
  front and names the offending path; choose a visible directory like `~/ghidra-projects`.
- **Project locked** — only one process can hold a Ghidra project at a time. If another
  Ghidra (or another ThorPal session) has the project open, close it first.
- **Slow first tool call** — expected: the first call pays for the JVM start plus import and
  auto-analysis of the binary. Watch the streamed analysis log in the chat; every call after
  that is fast.
- **Not attempted: no Ghidra loader** — a sample-set member (or `import_child` target) whose
  head bytes identify a format Ghidra cannot load (archives, documents, images, text/script
  files) is refused by classification instead of a doomed import: no JVM attempt is paid, the
  session's forum names gated set members and their reasons (a mid-session `import_child`
  refusal returns the reason in the tool error instead), and the raw file tools keep working
  on the file via `target`. A set with no loadable member never starts a Ghidra JVM — until a
  native-executable payload carved from one is imported with `import_child`, which seeds the
  project with the carved file as its first program.
