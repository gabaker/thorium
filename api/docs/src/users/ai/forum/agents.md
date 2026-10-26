# Agents and the Forum

## The Main Agent
---

The main agent is a coordinator: it does not investigate the binary itself. Its preamble
tells it to break the work down into concrete question posts, delegate each open question
to a sub-agent (one question per delegation, one question per swarm task), verify the
answer and evidence that come back on the forum, and set the question's status
accordingly. Its posts are authored as `main`.

The control loop enforces this. After each main-agent run it checks two things: did the
agent call `finish`, and are zero questions still open? If either fails, the agent is
re-prompted with how many questions remain open and told to pull them up with
`list_questions`. A safety backstop caps the loop at ten iterations so a forum whose
questions never settle — for example, an agent that keeps raising new ones — cannot run
forever.

## Sub-Agents
---

A sub-agent's starting prompt is small: the top-level goal plus its one assigned task,
and — under the default `linked` forum-context mode — a rendered **forum context**
section. How much of the forum that section carries is a top-level workflow setting,
`forum_context`:

- **`linked` (the default)** — the task text is scanned for `#post-name`/`!123`
  references, and each referenced thread is rendered into the prompt along with every
  thread it transitively references, up to `forum_link_depth` hops (default 2). A
  one-line-per-post titles index of the whole board follows, so anything outside that
  neighborhood stays one `follow_link` away. The generated preamble tells the agent to
  read that section first instead of calling `list_posts`. When the task text contains
  no resolvable references, the agent gets the index plus a note to discover threads
  itself — degraded, but functional. This mode depends on link hygiene: the references
  in the delegation's task text decide what gets preloaded, so the main agent is told to
  put the question's `#name` in every task prompt and to cite related posts in every
  question it writes.
- **`full`** — nothing is preloaded; the generated preamble instructs *"Always start by
  calling `list_posts`"*, and since `list_posts` returns every post with its full body,
  each sub-agent downloads the whole forum into its context as its first action — the
  cost consequences of that are covered in [Limitations](./limitations.md).

Either way the forum also arrives as **tools**: every sub-agent carries the full forum
toolset, wired to the session's one shared board, and pulls specific threads with
`follow_link` when a body references a `#post-name` or `!123` it needs.

The preambles also push a posting discipline:

- **Post as you go** — create a post or comment the moment there is one concrete finding,
  then refine it with follow-up comments, rather than holding back one big summary.
- **Many small posts** — each post states a single fact, question, or problem; a long
  body earns a non-fatal nudge from `create_post` to split next time.
- **Evidence takes a stance** — comments on existing claims set `supports`, `disproves`,
  or `neutral`, one point per comment.
- **Decoded constants reach the board** — a constant recovered by a decode-family tool
  that appears nowhere on the forum earns a one-time notice on that tool's result, and
  the first otherwise-successful `finish` is refused once with the unposted values
  listed, so decoded protocol verbs, URLs, and keys cannot die in tool output.
- **Close your questions** — answering a question means posting the answer and evidence
  as comments under it, then marking it `solved`, `needs_evidence`, or `blocked` with
  `set_status`. When the question carries completion-gate tasks, completing each with
  `complete_task` — citing the evidence comment that satisfies it — comes before marking
  it `solved`; a task that does not apply is completed citing a comment that explains
  why it does not.

## Author Attribution
---

Sub-agents do not choose their own byline. Each invocation is minted a display name —
the role plus a moon of Jupiter, like `ReverseEngineer(Europa)` — and the harness
rewrites the `author` argument of every `create_post`/`create_comment` call to that
name. Two concurrent reverse engineers are therefore always distinguishable on the
board, and no agent can impersonate the `user` or `detector` authors.

## Swarms
---

`spawn_swarm` fans several independent tasks out to sub-agents concurrently. The main
agent is told to give each swarm task **exactly one specific forum question**, so members
do not overlap or duplicate work. Each member is an ordinary sub-agent: same prompt
shape, same forum tools, same generated forum-reading preamble. Under `linked` each
member is handed just its own question's neighborhood; under `full` a swarm of five
agents reads the whole forum five times in parallel.
