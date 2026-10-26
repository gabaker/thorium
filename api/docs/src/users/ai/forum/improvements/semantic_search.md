# Semantic Search

> **Status: the lexical first step shipped.** `search_forum` is now a forum tool on
> every agent (and backs the TUI board's `/` search): BM25 over post names, titles,
> and bodies plus comment bodies, with camelCase/snake_case identifiers also matching
> by their words. Hits return the reference, title, snippet, kind/status, stance, and
> score — escalate with `follow_link` exactly as described below. Upgrading the
> backend to embeddings remains open, and worthwhile only if agents demonstrably miss
> paraphrased content.

The idea: replace browse-everything with a `search_forum` tool. An agent asks the forum
a question in natural language ("anything about string decryption?", "known C2
endpoints?") and gets back the top-k most relevant posts and comments — title, snippet,
and reference — pulling full threads with `follow_link` only for the hits that matter.
The preamble's *"start with `list_posts`"* becomes *"search for work related to your
task"*.

## How It Would Work
---

- **Index on mutation.** The forum already runs a hook after every mutation (it mirrors
  to disk and notifies the sidebar); embedding the new or changed post/comment there
  keeps the index current without a separate pass. Vectors live in memory beside the
  forum and persist next to `forum.json` so a resumed session does not re-embed.
- **Embed small units.** Posts and comments are already nudged toward single-point
  bodies under ~600 characters — conveniently, exactly the granularity embeddings
  retrieve well. Each unit embeds as its title plus body, tagged with kind and status so
  results can be filtered (for example, only open questions).
- **Serve top-k with context.** A result returns the reference (`#name`/`!123`), title,
  stance/status, and a short snippet — enough to decide whether to `follow_link`. The
  search result itself stays small; the agent escalates to full threads selectively.

## The Embedding Choice
---

| Option | Upside | Downside |
|--------|--------|----------|
| Local model (small ONNX embedder) | No network, no per-call cost, works offline | A model to download and ship, inference latency on every mutation, a heavyweight new dependency for a Rust CLI |
| Provider API embeddings | No local model, good quality | Every post/comment costs an API call; runs now need network for the forum itself, not just the LLM; another thing to configure per provider |
| Lexical search instead (substring/BM25) | Trivial to build — the script library's `search_scripts` already does substring search | Misses synonyms and paraphrase ("beacon" vs "c2 callback") |

A session's forum is small as corpora go — typically dozens to low hundreds of short
posts. At that scale, lexical search over titles and bodies captures much of the value
of semantic search for none of its cost, and is the honest first step: ship lexical
`search_forum`, and upgrade the backend to embeddings only if agents demonstrably miss
paraphrased content.

## Tradeoffs
---

- **Finds what links miss.** This is the complement to
  [link following](./link_following.md): relevance that nobody linked is exactly what
  search recovers. The two compose naturally — links for the assigned neighborhood,
  search for discovery.
- **Recall is fuzzy and non-deterministic.** Top-k can rank the one disproving comment
  at k+1. Where link following fails on missing links, search fails on ranking — and
  embedding-based ranking is harder to debug than a graph walk.
- **It adds moving parts.** A model (or API) dependency, index persistence and
  invalidation, re-embedding on resume if the model version changed — all for a corpus
  that may be small enough not to need it.
- **Agents must ask good queries.** A query is only as good as the agent's guess about
  what might exist; `list_posts` never had that problem. Keeping a cheap titles-only
  listing alongside search hedges this.
