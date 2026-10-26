# Thorctl Analyze

Thorctl ships an AI-assisted reverse engineering capability called ThorPal, driven by the
`thorctl analyze` subcommand (`thorctl ai` and `thorctl si` work as aliases). Point it at a
local binary and a team of AI agents will analyze it with Ghidra: listing and decompiling
functions, chasing cross references, walking the call graph, and reading memory. The agents
share their findings on a live forum you can watch and take part in, and the run can end
with a markdown report written from everything that was learned.

Analysis happens locally on your machine; only the conversation with your configured LLM
provider leaves it. ThorPal needs two things to be useful:

- An AI provider (OpenAI, a LiteLLM gateway, Anthropic, or AWS Bedrock)
- A local Ghidra installation for the agents that drive Ghidra

The tools the agents can call are documented in [Tools](./tools.md), the shared forum the
agents coordinate through has its own section under [Forum](./forum/forum.md), and the
Ghidra integration is covered under [Ghidra](./ghidra/ghidra.md).

## Quick Setup
---

The fastest way to get both is the interactive setup wizard:

```bash
$ thorctl analyze setup
```

It walks through everything in one pass: logging into Thorium when no config exists yet,
picking an LLM provider and entering its credentials (with an optional live connection
test), detecting an existing Ghidra install or downloading the latest release from GitHub,
checking for the full JDK Ghidra needs, and choosing the Ghidra projects directory. Nothing
is written until the final review step is confirmed. Offline boxes can install Ghidra from
a release zip already on disk with `--ghidra-zip <FILE>`, and `--ghidra-version`/
`--ghidra-dir` pin the release and the install location.

Everything the wizard does can also be configured manually, as described below.

## Configuring a Provider
---

Providers are configured in the `ai` section of your Thorctl config (`~/.thorium/config.yml`).
Each provider's settings live in a block named after it: `openai`, `litellm`, `anthropic`, or
`bedrock`. When only one block exists it is used automatically; with more than one, set
`provider` to pick the active one.

```yaml
ai:
  provider: anthropic
  anthropic:
    api_key: sk-your-key
    model: claude-sonnet-5-5
  ghidra_install: /opt/ghidra
```

The provider blocks support:

- `openai` — `api_key` and `model`, with an optional `endpoint` for OpenAI-compatible servers
- `litellm` — a gateway `endpoint` and model alias, with an optional `api_key` for
  authenticated gateways
- `anthropic` — `api_key` and `model`, with an optional `endpoint`
- `bedrock` — a `model` id, authenticating through the standard AWS credential chain by
  default; `region`, `profile`, or an explicit access key pair pin it down, and
  `mantle: true` targets Bedrock's Anthropic Messages API endpoint for models only served
  there

Older configs that set flat `endpoint`/`api_key`/`model` keys under `ai` keep working and are
treated as a `litellm` provider.

Every provider block also accepts an optional `pricing` block (dollars per million tokens)
used for cost estimation; see [Cost Tracking](#cost-tracking) below.

To verify your provider works before starting a real analysis, run a connectivity check. It
sends a trivial hello prompt with no tools attached and prints the provider's full error if
anything is wrong:

```bash
$ thorctl analyze check
```

## Configuring Ghidra
---

Agents that use the Ghidra tools need a Ghidra installation. Set it once in your config:

```bash
$ thorctl config --ghidra-install /opt/ghidra
```

or override it per run with `--ghidra-install` (or the `GHIDRA_INSTALL` environment
variable). Ghidra projects are created under `~/ghidra-projects` by default; change that
with:

```bash
$ thorctl config --ghidra-projects /data/ghidra-projects
```

Note that Ghidra 12.1 and newer rejects project directories whose path contains any element
starting with `.` (like `~/.thorium`), so pick a visible directory. See
[Ghidra](./ghidra/ghidra.md) for how the integration works and more setup details.

## Chatting About a Binary
---

Start an interactive session by pointing `thorctl analyze chat` at a binary:

```bash
$ thorctl analyze chat /path/to/suspicious_binary
```

The recording below is a real end-to-end session over the evaluation corpus's
Stealc v2 sample, played at full length (only
idle gaps are compressed): the `ai` settings are persisted into the default thorctl
config once, the full reverse-engineering prompt the
evaluation uses is pasted straight into the home screen,
the agents run the whole analysis — about 41 minutes and roughly $170 — and after a
look at the usage overlay the session ends by writing its report with `/report`.
[Reports](#reports) shows resuming the saved session later.

<link rel="stylesheet" href="../../static_resources/asciinema/asciinema-player.css">
<div id="cast-analyze-chat"></div>
<script src="../../static_resources/asciinema/asciinema-player.min.js"></script>
<script>
  AsciinemaPlayer.create('../../static_resources/asciinema/analyze-chat.cast',
    document.getElementById('cast-analyze-chat'),
    { cols: 120, rows: 34, idleTimeLimit: 2, poster: 'npt:0:10', preload: true });
</script>

This opens a terminal UI where you can converse with ThorPal about the binary while its
agents work. The chat supports:

- **Normal and insert modes** — `i` focuses the prompt box for typing (INSERT) and `Esc`
  leaves it (NORMAL) for scrolling, Space shortcuts, and the overlays (the Space menu in
  the UI lists the keys); closing an overlay always returns to normal mode
- **Collapsible tool calls** — every tool call the agents make shows up as a line in the
  chat log; click one to expand its arguments and result
- **Streaming log windows** — long-running tool calls (like the initial Ghidra analysis or
  a running Rhai script) stream their live output in a small window under the call while it
  runs
- **The forum sidebar** — a live view of the posts and questions the agents are raising and
  resolving on the shared [forum](./forum/forum.md), including each question's task
  checklist (`[ ]`/`[x]`, with the comment that completed each task); you can inject your
  own questions and comments to steer the analysis, and `t` adds a task to the selected
  question
- **Forum search** — `Space+s` (or `/` inside the sidebar) opens a helix-style split
  picker over the chat log: the query and the live-ranked hits on the left, a preview of
  the selected hit with the matched text highlighted on the right, and the tail of the
  chat log still visible beneath; `↑`/`↓` (or clicking a hit row) picks a hit, and
  `Enter` jumps to it in the sidebar, highlighting the matched text there until the
  selection moves (`Space+u` opens the usage overlay that used to live on `Space+s`)
- **Slash commands** — type `/` to open the command menu; `/report <path> [template]`
  writes a report from the current findings at any time
- **Analyst questions** — when the main agent hits a fork a human should decide, its
  `ask_analyst` tool opens a question modal over the chat and the turn waits for your
  answer: pick an option (`1`-`4`, or `↑`/`↓` and `Enter`), type your own (`o`), or press
  `Esc` to take the recommended default (recorded and flagged for review, exactly like an
  unattended run); see
  [Analyst Interaction Tools](./tools.md#analyst-interaction-tools)

### The views

Every view below is a `Space` shortcut in NORMAL mode (press `Esc` first if the prompt
has the keyboard), and `q` closes an overlay. The clips come from a finished evaluation
session over the corpus's Stealc v2 sample.

#### The forum sidebar — `Space`+`m`

The live message board where the agents post findings and raise questions: `j`/`k` walk
the posts, `Enter` folds and unfolds one, and `r`/`?`/`t` reply, ask a question, or add
a task to steer the analysis.

<div id="cast-chat-forum"></div>
<script>
  AsciinemaPlayer.create('../../static_resources/asciinema/chat-forum.cast',
    document.getElementById('cast-chat-forum'),
    { cols: 120, rows: 34, idleTimeLimit: 2, poster: 'npt:0:05', preload: true });
</script>

#### Forum search — `Space`+`s`

A helix-style split picker over the whole forum: type a query, watch the hits re-rank
live, and `Enter` jumps to the selected hit in the sidebar with the match highlighted.

<div id="cast-chat-search"></div>
<script>
  AsciinemaPlayer.create('../../static_resources/asciinema/chat-search.cast',
    document.getElementById('cast-chat-search'),
    { cols: 120, rows: 34, idleTimeLimit: 2, poster: 'npt:0:05', preload: true });
</script>

#### The usage overlay — `Space`+`u`

Per-agent cost and activity: `Tab`/`j`/`k` select an agent, `←`/`→` switch the metric,
and `Enter` drills into that agent's own conversation.

<div id="cast-chat-usage"></div>
<script>
  AsciinemaPlayer.create('../../static_resources/asciinema/chat-usage.cast',
    document.getElementById('cast-chat-usage'),
    { cols: 120, rows: 34, idleTimeLimit: 2, poster: 'npt:0:05', preload: true });
</script>

#### The timeline — `Space`+`t`

A gantt of the whole run over worked time: one bar per swarm and agent, expandable in
place. `Tab`/`j`/`k` select a row; `Enter` on a swarm reveals its member sub-agents, on
an agent reveals its individual tool calls, and on a tool call opens a detail panel with
the call's status, arguments, and full result (`j`/`k` scroll it, `Esc` backs out).
`h`/`l` pan the time window, `+`/`-` zoom, and `0` re-fits the whole run.

<div id="cast-chat-timeline"></div>
<script>
  AsciinemaPlayer.create('../../static_resources/asciinema/chat-timeline.cast',
    document.getElementById('cast-chat-timeline'),
    { cols: 120, rows: 34, idleTimeLimit: 2, poster: 'npt:0:05', preload: true });
</script>

#### The traversal graph — `Space`+`g`

A map of the functions the agents have visited, with the static call edges around them
overlaid from background Ghidra queries. The camera follows the graph by default: it
frames every displayed function while that stays legible and tracks the newest ones once
the graph outgrows the zoom cap (`hjkl`/`+`/`-` take manual control; `0` hands the camera
back to follow mode). `r` replays the whole walk from the start (`<`/`>` change the
replay speed, `G` jumps back to the live state), and `H` switches to a visit-frequency
heat view.

<div id="cast-chat-traversal"></div>
<script>
  AsciinemaPlayer.create('../../static_resources/asciinema/chat-traversal.cast',
    document.getElementById('cast-chat-traversal'),
    { cols: 120, rows: 34, idleTimeLimit: 2, poster: 'npt:0:05', preload: true });
</script>

Useful flags:

```bash
$ thorctl analyze chat ./binary --name dropper-triage     # save the session under a chosen name
$ thorctl analyze chat --resume dropper-triage            # resume a saved session
$ thorctl analyze chat ./binary --disable-detectors       # skip the detector pre-seed pass
$ thorctl analyze chat ./binary --report ./report.md      # write a report when the run finishes
$ thorctl analyze chat ./binary --anonymize               # blind the model to the file's name/path
```

With `--anonymize` (also available on `thorctl analyze summary`), ThorPal stages a copy of the
binary under the Ghidra projects root named by its SHA-256 and analyzes that copy, so the
model never sees the original file name or any directory in its path — useful when a
suggestive name (`invoice_lumma.exe`, a family-named corpus folder) could bias the analysis
toward prior knowledge instead of evidence. The sample's hashes stay visible (they are
content-derived), and the staged copy is kept so `--resume` keeps working; delete
`<projects_root>/<sha256>/` to clean a sample's copy and its Ghidra projects together.
The pre-staging path is deliberately **not** recorded anywhere in the session store
(anonymize-at-rest): a blinded session's store identifies members by SHA-256 only, so it
can be shared without leaking the operator's naming. Keep your own sha→path notes if you
need to trace a staged sample back to its source.

## Sessions
---

Every chat is persisted message-by-message under `~/.thorium/sessions/<NAME>`, so a crash or
an intentional exit loses nothing. The session folder also holds a `scripts/` directory with
every Rhai analysis script the agents wrote during the run (see
[Scripting](./scripting.md)), so you can read and reuse them after a run. Resume a session with `--resume <NAME>`: the binary,
Ghidra settings, and workflow all come from the saved session, the full conversation and
forum are replayed, and token/cost totals carry over. API keys and other secrets are never
written into the session; they are re-read from your live config on resume. The same goes
for the workflow's [MCP servers](./tools.md#mcp-tools): their `${VAR}` references are
expanded again from the live environment, Thorium MCP credentials are re-resolved from
your thorctl config, and every referenced server must be reachable again to resume.

If you do not pass `--name`, the session starts under an auto-generated placeholder name and
the model renames it to a short, descriptive name based on its findings before it finishes.

## Detectors
---

Before the agents start, a set of static detectors sweeps the binary for common malware
tells — defense evasion, command-and-control and network communication, encoded strings,
packing, sandbox checks, and more — and seeds the forum with one post per flagged function,
each backed by evidence comments. This gives the agents (and you) a prioritized starting
point instead of a blank page. Skip the pass with `--disable-detectors`.

## Reports
---

ThorPal can write a markdown report from the full transcript and forum. There are three ways
to get one:

- Pass `--report <path>` to `thorctl analyze chat` to write it automatically when the agent
  finishes its analysis
- Type `/report <path> [template]` in the chat to write one at any point
- A copy is always saved as `report.md` in the session store when a report is generated

When the run recorded analyst asks or review flags, the report's forum-evidence appendix
carries them as **Analyst decisions** and **Needs analyst review** — ahead of the finding
dumps, so the human-attention queue survives the appendix size budget first.

The recording below resumes the finished [Stealc v2 session](#chatting-about-a-binary)
with `--resume` — the saved conversation and forum replay instantly, at no model cost —
and writes its report with `/report`:

<div id="cast-resume-report"></div>
<script>
  AsciinemaPlayer.create('../../static_resources/asciinema/resume-report.cast',
    document.getElementById('cast-resume-report'),
    { cols: 120, rows: 34, idleTimeLimit: 2, poster: 'npt:0:08', preload: true });
</script>

The report's structure comes from a markdown template. The builtin template covers an
executive summary, a binary overview, key capabilities and findings with evidence, network
and I/O indicators, a risk assessment, and open questions. Supply your own with
`--report-template <path>`, or persistently with:

```bash
$ thorctl config --report-template /path/to/template.md
```

## One-Shot Summaries
---

When you just want an executive summary without an interactive session:

```bash
$ thorctl analyze summary /path/to/binary
```

It accepts the same `--ghidra-install` and `--workflow` overrides as `chat`.

## Batch Runs
---

`thorctl analyze batch` drives a whole evaluation campaign of headless chats from one manifest:
it stages samples out of password-protected dataset zips, launches sessions through an
adaptive rolling pool, recovers failures automatically off the headless exit-code contract,
and regenerates the evaluation chapter's figures. Unattended runs are never paused by the
agent's `ask_analyst` questions: with no TUI attached each ask immediately takes its
recommended default, is recorded, and is auto-flagged for review (`[ask]`/`[review]` lines
on stderr); pin `analyst_asks: false` in the workflow (or pass `--no-analyst-asks`) to
remove the tools entirely for strict A/B comparability with older runs. It talks only to the configured LLM
provider — no Thorium cluster is involved — and the `--config` it runs with must carry a
populated `ai:` block, since every child session inherits it. An evaluation lives in one
directory holding a `batch.yml` plus the `runs/` tree the tool maintains:

```yaml
prompt_file: prompt.md           # or inline `prompt: |`; exactly one is required
dataset:                         # only needed when units reference samples by sha256
  root: /datasets/MalwareSetV2
  password: infected             # the default
blind: true                      # sessions run under shuffled sNN aliases (default)
units:
  - name: ransom32               # evaluator-side label; never model-visible when blind
    samples:                     # primary first; 64-hex entries are dataset sha256 refs,
      - 6024e9b1ceeb59f2…        # anything else is a path to an already staged file
      - 9dd7855537fe2891…
```

```bash
$ thorctl analyze batch run --dir /path/to/batch        # start or continue the latest run
$ thorctl analyze batch run --dir /path/to/batch --new-run   # start the next run number
$ thorctl analyze batch run --dir /path/to/batch --max-concurrency 2  # cap the pool this invocation
$ thorctl analyze batch status --dir /path/to/batch     # per-unit status, usage, and cost
```

The recording below is a real two-unit run: the
Stealc v2 and
QUICKBIND samples from the
evaluation corpus are staged by sha256 out of
their dataset zips and evaluated end to end with the full reverse-engineering prompt.
Idle time is compressed for playback; the pool really ran for about 51 minutes and the
two sessions together cost about $219.

<div id="cast-analyze-batch"></div>
<script>
  AsciinemaPlayer.create('../../static_resources/asciinema/analyze-batch.cast',
    document.getElementById('cast-analyze-batch'),
    { cols: 110, rows: 30, idleTimeLimit: 2, poster: 'npt:0:15', preload: true });
</script>

Each unit becomes one headless `thorctl analyze chat` session (multi-sample units pass every
member, primary first) run with `--fresh-project --anonymize` and the manifest's prompt,
writing its report, stdout, and stderr under `runs/<N>/`. Sha256 sample refs are staged
automatically: the dataset zips are hashed once into `dataset-index.json` (refreshed
incrementally when zips change), each member is decrypted, verified against its sha256, and
staged read-only at `samples/<sha256>` — a content-addressed name that keeps family labels
out of every model-visible path. Under `blind: true` new units get shuffled `s01…`-style
aliases (the map is written to `runs/<N>/aliases.tsv`, evaluator-side only) and session
names come from `name_template`, by default `batch{run}-{alias}`.

The pool reproduces the evaluation runs' scheduler: concurrency starts at 4 and adapts
between a floor of 3 and a cap of 8 (`concurrency: { start, max, floor }`), backing off one
slot whenever the count of throttle markers across session logs grows and recovering one
every other poll window otherwise, with a 5s launch stagger and a free-disk hold
(`disk_free_min_gb`). When an `env_file` is set it is re-read immediately before **every**
spawn, so refreshed credentials reach each new session.

Recovery is automatic and bounded. A unit is complete exactly when it is *scoreable*: a
non-empty report plus a final `usage:` line. Otherwise, after each exit:

| Session outcome | Pool action |
|-----------------|-------------|
| Settled (exit 0) but the report is missing | `--resume --report-only` retries, up to `report_retries` |
| Pause outlived `--pause-wait` (exit 75) or failed with progress saved | `--resume --continue` with freshly sourced credentials, up to `max_retries` |
| Failed with nothing spent (a stillborn session) | store wiped, relaunched fresh |
| Interrupted (exit 130) during shutdown | left resumable; the next `batch run` continues it |

Re-invoking `batch run` continues a run: completed units are skipped, failures retry on
their remaining budgets (`--retry-exhausted` re-arms spent ones), sessions still running
from a crashed invocation are adopted by pid, and units newly added to the manifest are
staged and launched — that is also how a started run is extended. A launched unit's sample
list is immutable; changing it is an error (add a new unit instead). `--dry-run` prints the
reconciled plan without staging or spawning, `--units <names…>` narrows an invocation,
`--run <N>` pins `batch run` or `batch status` to a specific run number instead of the
latest, `--max-concurrency` caps live concurrency below the manifest's maximum for one
invocation, and
Ctrl-C drains gracefully: children finish their sessions (bounded), everything is left
resumable, and the pool exits 130. For long campaigns run it under `nohup` or a terminal
multiplexer.

Evaluation progression figures are regenerated from an evaluation page's Scoreboard
table with:

```bash
$ thorctl analyze batch figures path/to/evaluation.md
```

It rewrites the two SVGs and the spliced aggregate table idempotently (`unchanged` when
nothing moved); `--check` verifies without writing.

## Cost Tracking
---

The status bar at the bottom of the chat tracks the latest turn's context size, cumulative
input/output/total tokens, and a running estimated cost. Prices come from a built-in table
of well-known models; for models the table does not know (like a LiteLLM alias), or to
correct a rate, add a `pricing` block to your provider's config:

```yaml
ai:
  litellm:
    endpoint: http://gateway:4000
    model: my-sonnet-alias
    pricing: { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }
```

Each field is dollars per million tokens, and a partial block tweaks just the rates it
names.

## Reliability and Logs
---

Transient provider errors (rate limits and 5xx responses) are retried automatically with
jittered exponential backoff for up to roughly five minutes, shown as a single in-place
retry status line in the chat rather than an error. A file log of the whole run is written
to `~/.thorium/logs/thorctl-ai.log` for troubleshooting.
