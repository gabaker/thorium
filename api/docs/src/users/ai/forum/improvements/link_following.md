# Scoped Reading via Link Following

> **Implemented.** This design ships as the default: the top-level workflow setting
> `forum_context: linked` enables it (and is the default), `forum_context: full` opts
> back into the old whole-forum reads, and `forum_link_depth` sets the hop cap
> (default 2; 0 preloads the assigned thread alone). The settings live in the workflow,
> which is persisted with saved sessions, so a resumed run keeps its mode — and a
> session saved before the fields existed resumes into `linked`.

The idea: a sub-agent is already assigned one forum question — so hand it that question's
*neighborhood*, not the whole board. The forum is a graph: comments point at their
parents, and bodies reference other posts and comments as `#name`/`!123` free text. The
subgraph reachable from the assigned post is, by construction, the evidence trail the
task sits in.

## How It Works
---

- **At spawn**, the goal and task text are scanned for `#name`/`!123` references, and
  each referenced thread — the post, its nested comments, and their stances — is
  rendered into a `# Forum context` section of the sub-agent's starting prompt. A
  comment reference resolves to the post its thread hangs under, so the context always
  carries the claim being argued, not an isolated subtree. When the task text contains
  no resolvable references, the agent gets the index below plus a note to discover
  threads itself.
- **Expand transitively.** The rendered bodies (post and comments alike) are scanned for
  further `#name`/`!123` references, and each referenced thread is pulled too, up to
  `forum_link_depth` hops (default 2). Detector-seeded questions already cite the
  statement posts they build on, so the evidence behind a seeded question comes along
  automatically. Dangling references are skipped silently, and the walk is deterministic:
  roots render in task order, then each hop level name-sorted.
- **The preamble follows the mode.** Every sub-agent gets a generated *"Reading the
  forum"* preamble section matching the run's mode: under `linked` it says the assigned
  thread and everything it references is in the prompt, to use `follow_link` for any
  other reference, and the post index to check for related work — explicitly superseding
  any `list_posts`-first instruction an older workflow preamble may still carry. The
  forum tools all stay — nothing is taken away, only the default read shrinks.
- **A cheap index ships too.** The name, kind (with question status), title, and author
  of every post (no bodies) closes the spawn context. That one-line-per-post index is
  small, keeps the agent aware of what else exists, and gives `follow_link` targets.

## Why It Is Attractive
---

- **Deterministic and dependency-free.** The subset is a pure graph walk over data the
  forum already has — no model, no index, no new crate, and the same inputs always
  produce the same context.
- **Big savings exactly where the cost is.** Late-run spawns, the most expensive reads
  today, become cheap: a question's neighborhood grows with the *question*, not with the
  run.
- **It strengthens an existing norm.** The preambles already teach reference syntax and
  `follow_link`; detector questions already cite their evidence. The forum's linking
  culture is half-built.

## Tradeoffs
---

- **Unlinked relevance is invisible.** If another agent disproved a related claim but
  nobody linked the two threads, the sub-agent will not see it. Quality now depends on
  link hygiene — chiefly the main agent's, since it writes the question posts.
- **Link-poor forums degrade silently.** Nothing errors when links are missing; the
  agent just works with tunnel vision. The failure mode is subtle wrong answers, not
  loud ones.
- **Depth needs a cap, and caps cut.** A hop limit keeps dense neighborhoods bounded,
  but a two-hop-away disproof is as real as a one-hop one.

## Mitigations
---

- Always ship the titles-and-status index described above — it is a few tokens per post
  and converts "invisible" into "one `follow_link` away". *(Implemented: the index closes
  every linked-mode spawn context.)*
- Teach the main agent (via its preamble) to cite related posts in every question it
  writes, since question-writing is where the links that matter are made. *(Implemented:
  under `linked` the main agent's preamble says to put the question's `#name` in every
  delegation and to cite related posts and evidence comments in every question post.)*
- Pair with a discovery mechanism — lexical or [semantic search](./semantic_search.md) —
  so an agent that suspects more context exists can go find it. *(Not yet implemented;
  the titles index plus `follow_link` is the discovery path today.)*
