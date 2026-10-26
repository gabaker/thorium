# Other Options

Smaller changes that cut forum token cost without a retrieval system. Most compose with
[link following](./link_following.md) and [search](./semantic_search.md) rather than
competing with them.

## Titles-Only `list_posts`
---

Likely the cheapest large win: have `list_posts` return each post's name, title, author,
kind, and status — but not its body — and let agents fetch bodies with `follow_link`.
The browse-first workflow and preambles stay exactly as they are; the first-call dump
shrinks from every body ever written to one line per post. The tradeoff is an extra
tool-call round trip per post an agent actually wants to read, which is also the point:
reads become proportional to interest instead of forum size. A `full: true` argument
could preserve today's behavior for the rare caller that wants it.

## Filters and Pagination
---

Give `list_posts` the filtering `list_questions` already has — by kind, status, or
author — plus a limit/offset. Cheap to build and deterministic, but it makes the agent
guess which filter hides nothing important: filtering by `author: detector` or
`status: unsolved` is only safe when the agent already knows what it is looking for. As
a safety rail under titles-only listing it is useful; as the primary scoping mechanism
it is weak.

## A Maintained Digest
---

Keep a running summary of the forum — a few hundred tokens of "what we know so far, what
is open, what was disproved" — and give sub-agents the digest instead of the board. It
could be refreshed by the main agent (which reads the forum anyway) at delegation time,
or by a cheap model after every few mutations. This bounds spawn cost by the digest size
regardless of forum growth, which no other option here does. The tradeoffs are real,
though: a digest goes stale between refreshes, summarization costs tokens itself, nuance
(exact addresses, stance structure, who disproved what) is precisely what summaries
drop, and a wrong digest misleads every agent that trusts it.

## Hard Size Caps
---

Turn the advisory ~600-character split notice into a hard limit on body size (with a
generous ceiling), and consider a cap on total posts before `create_post` demands
consolidation. This does not scope reads at all — it just slows forum growth so every
read costs less. Cheap, crude, and effective in aggregate; the risk is agents fighting
the limit by splitting one finding across awkward fragments.

## Prompt-Cache-Friendly Reads
---

Providers discount cached prompt prefixes heavily, and a sub-agent's forum dump is a
large, stable block — but today each agent gets it mid-conversation after its own unique
prompt, so caches rarely line up across agents. Rendering the forum into a stable,
deterministic block placed before the per-agent material (and only appending to it) would
let concurrent swarm members share a cache hit on the same forum content. This reduces
cost, not context size: every agent still carries the whole forum in its window, so it
helps the bill more than the model's attention.
