# How It Works

## Posts and Comments
---

A **post** is a top-level item with a unique, space-free `name` (its `#name` reference),
a short title, a body, and an author. A **comment** replies to a post or to another
comment, gets a numeric id (its `!id` reference), and carries a **stance** describing how
its evidence relates to whatever it replies to:

| Stance | Meaning |
|--------|---------|
| `supports` | The comment's evidence backs the claim it replies to |
| `disproves` | The comment's evidence contradicts the claim it replies to |
| `neutral` | The comment adds context without arguing either way |

Both posts and comments are typed by **kind**: a `statement` (a fact, claim, or finding)
or a `question` (something that must be answered). Only questions carry a status; a
statement can never hold a meaningless one.

## Questions and Statuses
---

Every question tracks its resolution:

| Status | Meaning |
|--------|---------|
| `unsolved` | No answer yet — a fresh question always starts here |
| `solved` | Answered, with enough supporting evidence to accept |
| `needs_evidence` | Answered, but the answer is not yet trusted |
| `blocked` | Cannot be answered with the tools or information available |

A question counts as **open** while it is `unsolved` or `needs_evidence`; `solved` and
`blocked` are closed. The open count is what gates the end of a run: the `finish` tool
refuses while it is non-zero, and the control loop re-prompts the main agent with the
number of questions still open until it reaches zero.

## Tasks
---

A question may also carry **completion-gate tasks**: structured checklist items held
outside the body — a list of functions or addresses to check, values to verify — each
with a numeric id (shown as `task <id>`). Tasks attach at creation through `create_post`'s
optional `tasks` list or later with `add_tasks` (at most 20 per call), and only to
questions that are not already solved — never to statements, and a solved question must
be reopened with `set_status` before new tasks can attach (a gate added after the answer
was accepted could never gate anything).

Tasks are a hard gate on `solved`: `set_status` refuses to mark the question solved while
any task is incomplete, listing exactly which remain. `blocked` stays un-gated as the
escape hatch for questions that truly cannot be answered. Completing a task with
`complete_task` requires citing the comment (`!id`) that contains the information
satisfying it — post the evidence comment first — and the completion records both that
citation and who completed it, so every checked box carries an audit trail. Completions
are permanent. A task that does not apply is completed the same way: post a comment under
the question explaining why, and cite that.

[Detector seeding](#detector-seeding) attaches tasks too: a seeded question may carry up
to four concrete check tasks alongside its body.

## Analyst Asks and Review Flags
---

Two more record types live on the forum beside posts, comments, and tasks — both aimed at
the human analyst rather than at other agents (see
[Analyst Interaction Tools](../tools.md#analyst-interaction-tools)):

- **Analyst asks** — the main agent's `ask_analyst` questions, recorded only once
  resolved: the question, its 2-4 options, which was recommended, which was chosen, and
  by whom (`analyst`, `analyst_other` for a typed free-text answer, `auto_default` when
  nobody was attached, or `declined`). In the chat TUI the question arrives as a modal
  and the turn waits for the answer; unattended runs take the recommended default
  immediately.
- **Review flags** — any agent's `flag_for_review` markers: a short title, the reason
  human eyes are needed, and optionally the post or comment the flag anchors to. An ask
  that resolves without a live answer auto-files a `system` flag pointing back at it.

Neither record ever gates `finish`: asks resolve synchronously and flags are a review
queue, not questions. Both persist in `forum.json`, survive resumes, show on the message
board, and render into the report appendix's **Analyst decisions** and
**Needs analyst review** sections.

## The Tools
---

Every agent carries the same nine forum tools (also listed in [Tools](../tools.md)):

| Tool | What it returns |
|------|-----------------|
| `create_post` | The created post, plus a non-fatal "split this up" notice when the body runs past about 600 characters |
| `create_comment` | The created comment; the parent must already exist |
| `add_tasks` | The created tasks; the target must be a question |
| `complete_task` | The completed task, recording who completed it and the cited evidence comment |
| `list_posts` | **Every post on the forum**, name-sorted, with full bodies — no comments, no filtering, no pagination |
| `follow_link` | The post or comment a `#name`/`!id` reference points at, with its full nested reply thread |
| `set_status` | The updated question and its thread, after changing its status; `solved` is refused while the question has incomplete tasks |
| `list_questions` | Question posts and comments matching a status filter (`open`, `all`, or a specific status), each with its full thread |
| `finish` | Main agent only; an error while any question is open — and a one-shot refusal when decoded constants were never posted — otherwise an acknowledgement the control loop re-confirms |

The harness also watches the opposite direction — values recovered in tool output that
never reach the board. When a decode-family tool (`decode_bytes`, `byte_transform`,
`pairwise_sub`, `strings_range`, `api_hash_match`, `run_script`, `emulate_function`)
recovers a constant that appears nowhere on the forum, the tool's result carries a
one-time, non-fatal notice asking the agent to post it if it is meaningful (a C2 protocol
verb, URL, key, or campaign id). As a backstop, the first `finish` call that would
otherwise succeed while such constants remain unposted is refused once, listing up to five
of them verbatim; the refusal is a one-shot latch, so a run can never deadlock on it.

The tools are not the only way forum content reaches an agent: under the default
`forum_context: linked` workflow setting, each sub-agent's starting prompt carries its
assigned question's thread, every thread it transitively references (up to
`forum_link_depth` hops, default 2), and a titles-only index of every post — see
[Agents and the Forum](./agents.md). Setting `forum_context: full` preloads nothing and
has agents start with `list_posts` instead.

## One Forum per Session
---

Each session owns exactly one forum, held in memory and shared by every agent in that
session — the forum tools all operate on the same board, so a post made by one sub-agent
is immediately visible to every other. Two concurrent sessions never share forum data.

The forum is mirrored to disk after **every** mutation, as `forum.json` inside the
session folder (`~/.thorium/sessions/<NAME>`), alongside a human-readable boxed rendering.
Resuming a session restores the posts, comments, and comment-id counter exactly, so a
resumed run continues the same discussion. Every post and comment also carries a
monotonic revision stamp, which is how the sidebar jumps to the most recent activity.

## Detector Seeding
---

Before the agent loop starts, the [preseed detectors](../ghidra/detectors.md) sweep the
binary and stock the forum, attributed to the author `detector`:

- One **statement post per flagged function** (plus a file-level bucket), with one
  evidence comment per finding under it. These are the exhaustive record.
- Up to **six derived open questions** from the strongest findings (is it packed? what
  does this host check gate?), each citing the statement posts it builds on. Questions
  are capped because every open question forces the main agent to delegate and resolve
  it — an unbounded queue would spend the tokens the static pass is meant to save.

Skip the whole pass with `--disable-detectors`.

## You on the Forum
---

The chat sidebar is a live view of the forum, and it is interactive: you can raise your
own questions and comment on any thread. Your items are attributed to the author `user`
and look exactly like agent posts, so a question you inject is picked up and delegated by
the main agent like any question it raised itself — and it blocks `finish` until it is
resolved.

The agent can also address you directly: an `ask_analyst` call opens a question modal
over the chat and parks its turn on your answer, and the sidebar shows every recorded
ask and review flag (anchored flags inside their post's card, the rest in the
"Analyst asks & review flags" tail).

## Reports
---

Report generation renders the **entire** forum — every post with its full nested comment
thread — into the report prompt together with the conversation transcript. The forum is
therefore the evidence base of the final report, which is a good reason to keep posts
small and well-linked while a run is going.

Recorded analyst asks and review flags follow the threads into that render and into the
report's deterministic forum-evidence appendix, as **Analyst decisions** and
**Needs analyst review** — placed after the question sections and ahead of the finding
dumps, so a fork taken on an auto-default or a flagged conclusion is never invisible to
the report's reader.
