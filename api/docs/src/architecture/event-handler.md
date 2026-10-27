# Event Handler
---
The event handler in Thorium is responsible for triggering reactions based on
events in Thorium. An event in thorium is an action taking place like:
- Uploading a file/repo
- Creating tags

When these event happen they are pushed into a stream in redis. The event handler
then pops events from this stream and determines if the conditions for a pipeline
trigger have been met. If they have then a reaction will be created for the user
whose event met this triggers conditions. A single event can trigger multiple
distinct triggers.

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

### Event Handler FAQ's
---

### Is there a delay between events being created and being processed
Yes, the event handler trails live events by 3 seconds (workflow graph events by
`thorium.events.workflow_graph_delay`, 10 seconds by default; model refresh events don't wait). This is to ensure that
Scylla has a chance to become consistent before the event handler process an
event. Event though event data is stored in Redis the event-handler often has
to query for additional data to determine if a trigger's conditions have been
met. This data is stored in Scylla and so requires some time to become
consistent.

### What stops an infinite loop in events?
Triggers have an configurable depth limit meaning any events that reach that
limit will be immediately dropped instead of processed.

### Can I replay events?
No, once an event is processed it is dropped and cannot be replayed.
