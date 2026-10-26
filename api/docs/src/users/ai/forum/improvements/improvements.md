# Potential Improvements

This section documents designs for cutting the forum's token cost, with their
tradeoffs, so the direction can be chosen deliberately. One of them is now
implemented: [link following from the assigned post](./link_following.md) ships as the
default `forum_context: linked` workflow setting (`full` opts back into the old
whole-forum reads). The rest remain unimplemented designs.

## The Problem, in Tokens
---

Every sub-agent starts by reading the entire forum, and that read is paid many times
over (see [Limitations](../limitations.md)): once per spawn, multiplied by swarm sizes,
and re-billed as input context on every turn the agent takes afterward. The structural
mismatch is that **assignment is already scoped — reading is not**. A sub-agent gets
exactly one forum question as its task, but its view of the forum is everything ever
posted, almost all of it irrelevant to that question.

Any fix therefore has the same shape: give each agent a *relevant subset* of the forum
instead of all of it, plus a way to reach anything outside that subset on demand. The
designs differ in how the subset is chosen and what they risk leaving out.

## The Options
---

| Approach | Token savings | Complexity | Risk of missing context | Deterministic |
|----------|--------------|------------|-------------------------|---------------|
| [Link following from the assigned post](./link_following.md) | High | Low — no new dependencies | Misses relevant-but-unlinked posts | Yes |
| [Semantic search](./semantic_search.md) | High | High — embeddings model, index upkeep | Low, but recall is fuzzy | No |
| [Titles-only `list_posts`](./other_options.md) | Medium–high | Very low | Low — titles remain a full index | Yes |
| [Filters and pagination](./other_options.md) | Medium | Low | Medium — agent must guess the right filter | Yes |
| [Maintained digest](./other_options.md) | Medium | Medium — a summarization step to run and refresh | Medium — digest can go stale or omit nuance | No |

The approaches compose. The most promising combination is probably **link following for
the assigned neighborhood** (cheap, deterministic, exactly the evidence trail the task
sits in) plus **titles-only listing or lexical search for discovery** (so an agent can
still find relevant work that nobody linked), with semantic search held back until a
cheaper option demonstrably misses things.
