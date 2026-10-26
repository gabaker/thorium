# Limitations

The forum works well as a coordination and evidence structure, but its read side is
brute force. These are the known weak points; the
[Potential Improvements](./improvements/improvements.md) section documents ways to
address them.

## Every Sub-Agent Reads the Whole Forum (`full` mode)
---

This was the headline cost, and it is now opt-in: the default `forum_context: linked`
workflow setting preloads each sub-agent with just its assigned thread's neighborhood
plus a titles-only index (see [Agents and the Forum](./agents.md)), implementing
[link following](./improvements/link_following.md). Under `forum_context: full`, the
old behavior below still applies — and `linked` inherits that design's tradeoff:
relevance nobody linked is invisible until the agent discovers it through the index.

Under `full`, a sub-agent's first instructed action is `list_posts`, which returns
**every post with its full body** — there is no filter or pagination (`search_forum`
now offers relevance-ranked discovery, but the instructed full dump remains the
baseline read). That dump then sits in the agent's context for its entire run,
re-billed as input tokens on every subsequent completion the agent makes.

The cost compounds in three ways:

- **The forum only grows.** Detector seeding alone can stock dozens of statement posts
  before the first agent spawns, and agents are explicitly told to post early and often.
  A sub-agent spawned late in a run pays for everything posted before it.
- **Spawns multiply it.** Total forum-read cost is roughly the forum's size at each
  spawn, summed over every spawn — and a swarm of N agents pays the full price N times
  at once.
- **Context is re-billed per turn.** A 10,000-token forum dump in a sub-agent that makes
  30 tool calls is not 10,000 tokens; it is input context on 30 requests (softened, but
  not eliminated, by provider prompt caching).

Almost none of that content is relevant to the agent's one assigned question — the
assignment model is already "one specific forum question per sub-agent", but the reading
model is still "everything".

## Finding Things Is Lexical Only
---

> **Partially addressed:** `search_forum` now gives every agent (and the TUI's board)
> BM25 relevance search over post names, titles, and bodies plus comment bodies — see
> [Semantic Search](./improvements/semantic_search.md). The remaining gap is
> paraphrase recall: lexical search cannot connect "beacon" to "c2 callback" unless
> one of those literal terms was posted.

Before `search_forum`, `list_posts` was the only discovery tool, and it hides comments
entirely. Evidence lives in comment threads, so an agent that wanted to know whether a
claim was already disproved had to notice the right post in the full listing and walk
its thread with `follow_link`.

## Size Is Only Nudged, Never Enforced
---

The single size control is advisory: a post body past about 600 characters earns a
non-fatal "prefer several small posts" notice. Nothing stops a long post, caps total
forum size, archives stale threads, or collapses solved questions — a solved question's
full thread costs every later reader just as much as an open one.

## The Report Pays Too
---

Report generation embeds the entire rendered forum — every post and every nested
comment — in the report prompt. On a long run the forum can dominate that prompt, and
there is no summarization or filtering step before it.

## Smaller Rough Edges
---

- **Flat namespace** — post names live in one global namespace with no categories or
  boards; agents must invent non-colliding names, and a duplicate name is a hard error.
- **No editing** — posts and comments are append-only; a superseded claim can only be
  argued down with `disproves` comments, never amended or retracted.
- **Links are free text** — `#name`/`!123` references inside bodies are resolved only
  when an agent chooses to call `follow_link`; nothing validates them at write time, so
  a typo silently points nowhere.
- **The forum is per-session by design** — reusable, generic lessons stuffed into posts
  die with the session and inflate every later reader's context. Durable cross-run
  knowledge now has its own home, the [memory library](../memories.md); sample- and
  session-specific facts stay here.
