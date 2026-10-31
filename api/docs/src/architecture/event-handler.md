# Event Handler
---
The event handler in Thorium is responsible for acting on events in Thorium. An event
in Thorium is an action taking place like:
- Uploading a file/repo
- Creating tags
- Saving a tool result that contains scannable entities (Sigma scanning)
- Uploading a sample or widening its group visibility (YARA scanning)
- A hunt chunk becoming ready to execute

When these events happen they are pushed into per-kind queues in Redis. The event
handler runs one worker per event kind:

| Worker | Queue | What it does |
|--------|-------|--------------|
| `ReactionTriggers` | `ReactionTrigger` | Evaluates pipeline tag triggers and spawns reactions |
| `SigmaScanner` | `SigmaScannableResults` | Scans tool-result entities with stored Sigma rules |
| `YaraScanner` | `YaraScannableSample` | Scans sample bytes with the compiled YARA ruleset |
| `YaraHunter` | `YaraHuntChunk` | Executes one chunk of a user-created hunt |

For the trigger worker, a single event can satisfy multiple distinct triggers and each
spawns a reaction for the user whose event met the trigger's conditions. The scanning
and hunt workers are covered in depth in [Rule Scanning](./rule-scanning.md).

Each event handler process claims the events it pops under its own worker identity and
heartbeats every 10 seconds. A reaper reclaims the in-flight events of workers that
stop heartbeating (crashed pods) or hold leases too long (wedged pods), so events are
never stranded by a dead replica. This is what makes the event handler safe to run
with multiple replicas.

The event handler also runs an AI worker that keeps AI workflows and model catalogs current:

- **Workflow graph events.** Creating or editing a workflow, an AI agent, a toolset, a graph edge,
  or a tool's tags, and publishing or deprecating a tool version, queue `WorkflowGraph` events. The
  AI worker rebuilds the toolset instances and workflow instances they affect (toolsets first, so a
  workflow built in the same batch uses the new toolset instances), creates a new instance only when
  something that matters changed, and notifies the user who caused the build. These events wait
  `thorium.events.workflow_graph_delay` seconds (10 by default) so a burst of changes is handled
  together. See [AI Agents, Toolsets, and Workflows](../developers/agents_workflows.md).
- **Model refresh events.** Creating a model endpoint, changing its connection, rotating a
  credential, or asking with `thorctl models refresh` queues a `ModelRefresh` event. The AI worker
  lists the endpoint's models through the provider's API (five refreshes at a time) and reports them
  back, and the API merges them into the endpoint's catalog and bans. See [Models](../users/models.md).

Workflow graph and model refresh events never trigger reactions.

The event handler also scans tool results with sigma rules. When a tool uploads
results containing scannable entities (e.g. windows processes) an event is
created and the event handler runs all matching sigma rules against those
entities. When a rule hits it performs that rule's actions, which can either
flag the matched entity or spawn a reaction.

### Spawning reactions from sigma rule hits
A sigma rule can have a `SpawnReaction` action that spawns a pipeline against
the same data (sample or repo) that the originating reaction ran on. Repo
results only record the repo url, so a reaction spawned from repo results runs
against the repo's default checkout rather than the commit the originating
reaction ran on. Parts of
the matched entity can be passed to the pipeline as args using jq filters. For
example this action dumps the memory of any process a rule hits on by passing
the matched process's pid to volatility:

```json
{
  "SpawnReaction": {
    "group": "forensics",
    "pipeline": "volatility-dump",
    "args": {
      "volatility": {
        "positionals": ["\"memdump\""],
        "kwargs": {"--pid": [".pid"]},
        "switches": ["--verbose"]
      }
    }
  }
}
```

The args map is keyed by image name. Every positional and kwarg value is a jq
filter run against the metadata json of the entity the rule hit on (the same
json the sigma rule matched), while switches are passed through as literals.
Literal positional/kwarg values can be set with jq string literals like
`"\"memdump\""` and optional fields can be defaulted with jq's alternative
operator (e.g. `.parent_pid // 0`). Extraction fails closed: if any filter
errors, returns null, or yields no outputs then that spawn is skipped and
logged. A filter that runs past the render timeout is also skipped, and that
action is not rendered again until the event handler restarts. Spawned
reactions run as the user whose results were scanned and are
tagged with `sigma-rule:<rule name>`.

### Event Handler FAQ's
---

### Is there a delay between events being created and being processed
Yes, the event handler trails live events by 3 seconds (workflow graph events by
`thorium.events.workflow_graph_delay`, 10 seconds by default; model refresh events don't wait). This is to ensure that
Scylla has a chance to become consistent before the event handler processes an
event. Even though event data is stored in Redis the event-handler often has
to query for additional data to determine if a trigger's conditions have been
met. This data is stored in Scylla and so requires some time to become
consistent.

### What stops an infinite loop in events?
Triggers have a configurable depth limit (`thorium.events.max_depth`, default 5)
meaning any events that reach that limit will be immediately dropped instead of
processed. Tags written by YARA hits inherit the depth of the event that caused the
scan plus one, so hit-triggered pipelines also terminate at the depth limit. Sigma
rule scanning follows the same limit: result uploads carry the trigger depth of the
job that produced them, and a sigma rule hit refuses to spawn a reaction once that
depth reaches the configured max depth. Reactions spawned by a sigma rule hit run
one trigger level deeper than the event that caused them. Note that like tags and
children, the depth on results is stamped by the uploader (the Thorium agent), so
results uploaded by clients that do not stamp a depth (e.g. manual uploads) start a
fresh chain at depth 0. A fresh chain also restarts the loop guard: if a tool in a
sigma-spawned pipeline uploads its own results directly without stamping a depth
and those results match the same spawning rule, every pass re-enters at depth 0 and
the max depth never ends the loop — such tools must stamp the depth they ran at (as
the agent does) or the rule must not match its own pipeline's results.

### Can I replay events?
Successfully processed events are dropped and cannot be replayed. However, in-flight
events held by a worker that dies or shuts down are automatically returned to the
pending queue and processed again by another worker. This replay is bounded and safe
for scanning (hit writes converge on the same entities), but a reaction spawned right
before a crash can be duplicated.
