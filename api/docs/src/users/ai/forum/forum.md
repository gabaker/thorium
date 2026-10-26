# Forum

ThorPal's agents do not pass findings to each other directly — they share everything
through a Reddit-like forum. Every agent in a session (the main agent and each sub-agent),
the pre-run detectors, and you all read and write the same board. A post raises a single
question, claim, or finding; threaded comments attach evidence under it; and every
question carries a resolution status that the run cannot end without settling. Questions
can also carry completion-gate task checklists — concrete checks the run cannot mark the
question solved past. The forum
is also what you see live in the chat's sidebar, and it is the raw material reports are
written from.

Posts are referenced with GitLab-style syntax: `#post-name` for a post and `!123` for a
comment. That one convention is used everywhere — in free text inside bodies, and as the
literal argument format for the forum tools — so agents only learn it once.

This section covers [how the forum works](./how_it_works.md) (the data model, storage,
and seeding), [how the agents use it](./agents.md), its current
[limitations](./limitations.md) — most notably that every sub-agent re-reads the whole
forum when it spawns — and the [potential improvements](./improvements/improvements.md)
that could cut that cost, including
[scoped reading via link following](./improvements/link_following.md) and
[semantic search](./improvements/semantic_search.md).

## Why a Forum
---

A shared board solves three coordination problems at once:

- **Findings outlive the agent that made them.** Sub-agents are short-lived; their
  posts are not. A later agent (or a resumed session) picks up exactly where the
  evidence trail left off.
- **Claims are challengeable.** Every comment takes a stance — supports, disproves, or
  neutral — toward whatever it replies to, so a wrong claim collects disproving evidence
  instead of silently propagating.
- **The run has an honest stopping condition.** Questions are tracked, not vibes: the
  main agent's `finish` tool refuses while any question is still unsolved or has an
  answer without enough evidence to trust.
