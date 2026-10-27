# AI Agents, Toolsets, and Workflows

Thorium can run AI workflows on files and repos: one or more configured **AI agents** that call
tools and delegate to each other, run by a [harness](./harnesses.md). This page covers building
them with Thorctl. Running them is covered in [Running Tools and Workflows](../users/runs.md).

> The `thorctl agents` commands manage AI agents. They are unrelated to the `thorium-agent` job
> runner described in the admin docs.

### Concepts
---

| Term | Description |
| ---- | ----------- |
| AI agent | A configured AI agent: prompts, the models it accepts in priority order, model requirements, and limits. It calls tools and toolsets, may delegate to other agents, and runs in a harness. Agents are edited in place, not versioned |
| Toolset | A named set of agentic tools that agents attach as one unit, by explicit includes and an optional tag selector |
| Toolset instance | A constant snapshot of a toolset's resolved tools, named `<toolset>#r<revision>` |
| Workflow | The editable root that wires agents together: its agents, entry agent, limits, default models, and which levels may supply model auth (also called a workflow catalog) |
| Workflow instance | A constant snapshot of a workflow's resolved graph (agents, tool versions, toolset instances, tool bindings), named `<workflow>#r<revision>`. Only Ready instances run |

Agents follow a tool or toolset either at its latest version or pinned to one instance. Editing an
agent changes every workflow it is in; to change one workflow only, create a copy of the agent.

All of these are entities, so the same group rules apply as for tools: you need edit rights and the
`Entities` allow flag in every group to create one, and in all of its groups to change it. An
agent, toolset, or workflow can only point at things that are in all of its groups. Pass
`--extend-target-groups` to add the missing groups to the target instead, when you may edit it
there.

### AI agents
---

Create an agent from a YAML or JSON config file:

```yaml
system_prompt: You lead a malware triage
prompt: Triage the attached files and summarize the findings
models:                         # highest priority first
  - model_id: claude-opus-4
  - model_id: gpt-5
model_requirements:
  min_context_window: 100000
  capabilities: {tool_calling: true}
max_turns: 40
max_tools: 50
function_name: triage_lead      # needed when another agent delegates to this one
model_description: Leads a malware triage
tool_choice: Auto               # Auto, Required, None, or {Function: <name>}
```

```bash
thorctl agents create --config lead.yaml --name triage-lead --group analysts
```

Every field is optional. An agent with no `models` uses the workflow's default models. A run has no
per-run question: agents work from their prompts and the run's input files and repos.

Wire the agent up:

```bash
# call a tool at its latest version, or pinned to one version
thorctl agents link-tool triage-lead strings
thorctl agents link-tool triage-lead yara-scan --pin <TOOL_INSTANCE_ID>
# call every tool in a toolset
thorctl agents link-toolset triage-lead static-analysis
# delegate to another agent (the sub-agent needs function_name and model_description)
thorctl agents delegate triage-lead unpacker
# run in a harness, at its latest version or pinned with --pin
thorctl agents set-harness triage-lead my-harness
# list the agents in your groups, or see one in full
thorctl agents get
thorctl agents describe triage-lead
```

A sub-agent is offered to its parent as a tool and runs inside the same harness process; its
`function_name` can't collide with the parent's tools.

### Toolsets
---

Toolsets and workflows can't take a protected name such as `toolset`, `workflow`, or `tools` (the list is under
[Creating and publishing](./tools.md#creating-and-publishing)); existing ones with such a name keep working.

```bash
# a toolset with two tools and every agentic tool tagged area=static in all of its groups
thorctl toolsets create static-analysis --group analysts --tool strings --tool yara-scan \
  --selector area=static
# include or remove tools later
thorctl toolsets include static-analysis floss --pin <TOOL_INSTANCE_ID>
thorctl toolsets remove-tool static-analysis strings
# change or clear the selector
thorctl toolsets selector static-analysis area=unpack
thorctl toolsets selector static-analysis --clear
# only keep tools whose version declares no effects
thorctl toolsets read-only static-analysis on
# list the toolsets in your groups, or see one in full
thorctl toolsets get
thorctl toolsets describe static-analysis
```

`toolsets create --read-only` turns the read-only filter on when the toolset is created.

A selector only adds tools that carry the tag in every one of the toolset's groups. Only agentic
tools can be in a toolset, and tools without a latest version, or with effects when the read-only
filter is on, are left out with a reason.

### Workflows
---

```bash
thorctl workflows create malware-triage --group analysts \
  --agent triage-lead --agent unpacker --entry triage-lead \
  --default-model claude-opus-4 --max-alive-sub-agents 4
```

| Flag | Meaning |
| ---- | ------- |
| `--agent` | An agent in the workflow (repeatable) |
| `--entry` | The agent that receives the workflow's inputs first; it must be one of its agents and must run in a harness |
| `--default-model` | A model id for agents without their own models list, highest priority first (repeatable) |
| `--max-alive-sub-agents` | The most sub-agent invocations a run has going at once |
| `--model-auth` | A level that may supply model auth for the workflow's runs: `harness`, `workflow`, `group`, or `user` (repeatable; every level by default). `harness` and `workflow` are reserved and not used yet |
| `--no-auto-instance` | Don't build new workflow instances automatically |

Change it later with `thorctl workflows add-agent`, `remove-agent`, and `entry` (`--clear` clears
the entry agent). List the workflows in your groups with `thorctl workflows get`, and see one with its
agents, entry agent, settings, and latest instance with `thorctl workflows describe <WORKFLOW>`.

#### Workflow and toolset instances

The event handler builds instances for you. A new workflow instance is built when the workflow is
edited, an agent's config or links change, a tool an agent follows at latest gets a new latest
version, or a new Ready toolset instance appears; toolset instances are rebuilt the same way when
their tools change. A build only creates an instance when something that matters changed, and the
user who caused it gets a notification saying whether the new instance is Ready or Invalid (with
every error). Pinned tools never trigger a build. Builds wait about 10 seconds so a burst of edits is
handled together.

An instance is **Invalid** when, for example, the entry agent has no harness, a tool can't be bound
to the harness (see [Binding tools](./harnesses.md#binding-tools)), two tools share a function name,
or a limit is exceeded (32 agents per workflow, 128 tools per agent, 64 tools per toolset, and 8
delegation hops by default). Invalid instances never run; earlier Ready instances keep running.
The API also refuses a `link-tool` link that would give an agent more than twice the tool limit
(256 by default) in tools, and a `delegate` or workflow `--agent` link that would pass the agent
limit.

```bash
# list a workflow's instances and inspect one
thorctl instances get workflow malware-triage
thorctl instances describe "malware-triage#r3"
thorctl instances diff "malware-triage#r2" "malware-triage#r3"
# one agent of an instance with the full spec of each of its tools
thorctl workflows toolset "malware-triage#r3" --agent triage-lead
```

```bash
# ask for a new instance now instead of waiting for the event handler
thorctl instances request workflow malware-triage
thorctl instances request toolset static-analysis --wait-for instance-revision=4
# see what would be requested
thorctl instances request workflow malware-triage --dry-run
```

`instances request` only queues a build (`202`) and prints the root, the highest revision so far, and
the revision the build will probably create (other requests may be ahead of it). A revision is never
given out twice: a build whose write fails still uses up its revision, so a root's revisions can skip
numbers, and a `--wait-for instance-revision=N` naming a skipped revision times out. With `--wait-for`
(for example `instance-revision=4`) or `--watch` it then polls the instance listing and prints it
once the condition holds; `--timeout` bounds the wait and a timeout exits `3`.
The same route is `POST /api/entities/workflows/{id}/instance` (or
`/api/entities/toolsets/{id}/instance`); you need edit rights in all of the root's groups.

### Creating from flags or a spec
---

`agents create` takes the links as flags, so an agent can be wired up in one command, and `toolsets
create` and `workflows create` take a whole spec:

```bash
# an agent with its tools, toolset, sub-agent, harness, and model auth levels
thorctl agents create --config lead.yaml --name triage-lead --group analysts \
  --tool strings --tool yara-scan@1.0.0 --toolset static-analysis --sub-agent unpacker \
  --harness my-harness --model-auth group --model-auth user
# create a toolset or a workflow from a spec printed by `spec` (flags replace what the spec sets)
thorctl toolsets create --spec static-analysis.yaml --group analysts
thorctl workflows create malware-triage --spec triage.yaml --group analysts --entry triage-lead
```

On the command line a tool is `NAME` (its latest version) or `NAME@VERSION` (pinned), and a toolset
is `NAME` (its latest instance) or `NAME#rN` (pinned to that revision). A tool whose own name
contains `@` can't be pinned this way; use the spec or an id. Names are looked up in the groups of
the thing you are creating or editing, so the same tool name in an unrelated group doesn't make
a name ambiguous.

### Changing, unlinking, and deleting
---

Every one of these has a `spec` command that prints it as a document, an `update` command that
changes it without an editor, and an `edit` command that opens it in your editor (see
[Editing Resources](./editing_resources.md) for the flags they share and what the editor shows).
A spec round-trips: `spec` followed by `update --file -` is a no-change.

```bash
# print an agent, toolset, or workflow as YAML (or --format json, or write it with --output)
thorctl agents spec triage-lead
thorctl toolsets spec static-analysis --output static-analysis.yaml
thorctl workflows spec malware-triage --output triage.yaml

# change it from flags
thorctl agents update triage-lead --rename triage-chief --add-group reversers -d "Leads a triage"
thorctl agents update triage-lead --config lead.yaml --model-auth group
thorctl toolsets update static-analysis --auto-instance off --add-group reversers
thorctl workflows update malware-triage --max-alive-sub-agents 8 --default-model claude-opus-4

# change it from a document or single fields
thorctl agents update triage-lead --file lead.spec.yaml
thorctl agents update triage-lead --set config.max_turns=20 --unset description
thorctl workflows update malware-triage --set 'agents=[triage-lead,unpacker]' --dry-run

# change it in an editor
thorctl agents edit triage-lead
thorctl toolsets edit static-analysis
thorctl workflows edit malware-triage
```

The typed flags are turned into the same edits as `--set` and are applied before yours, so
`--add-group` followed by `--set` on the result works. `--add-group` and `--remove-group` are
relative to the current groups, so they can't be combined with `--file`. A few details worth
knowing:

- A document must list every list field (`tools`, `groups`, and for agents `toolsets` and
  `sub_agents`; for workflows `agents`), so a partial `--file` fails instead of silently emptying
  a list. To empty a list use `--set tools=[]`; `--unset tools` is refused.
- In a document an agent's tools are `{tool: NAME, version: LABEL}` (`version: null` follows the
  latest), its toolsets are `{toolset: NAME, instance: N}`, its harness is `{tool, version}`, its
  sub-agents are names, and `model_auth` is a list of `harness`, `workflow`, `group`, and `user`.
  `config` is the same mapping as the config file, and a key that isn't an agent setting is refused
  with the list of valid keys.
- A workflow's `entry_agent` is a name that must be one of its `agents`. Removing the entry agent
  from `agents` without clearing or moving `entry_agent` is refused.
- `workflows update --default-model` replaces the whole default model list (a model id that is
  already listed keeps its settings), and `--clear-default-models` empties it.
- `--extend-target-groups` is accepted by `update` and `edit` as well as `create`.

Remove links with the matching commands. They check the link first and fail naming it, because the
API would silently ignore a link that isn't there; a pinned link needs the same `--pin` it was
made with:

```bash
thorctl agents unlink-tool triage-lead strings
thorctl agents unlink-tool triage-lead yara-scan --pin <TOOL_INSTANCE_ID>
thorctl agents unlink-toolset triage-lead static-analysis
thorctl agents undelegate triage-lead unpacker
thorctl agents clear-harness triage-lead
```

Delete one with `delete`, which asks for confirmation at a terminal (`--skip-confirm`, alias
`--yes`, otherwise) and prints what it would do with `--dry-run`:

```bash
thorctl agents delete unpacker --dry-run
thorctl toolsets delete static-analysis --yes
thorctl workflows delete malware-triage
```

Thorium refuses to delete anything that something else still references by name or id (`409`
Conflict, and nothing is deleted). The message names the referrers you can see, at most ten and
then a count, and counts the ones in groups you can't see without naming them:

| To delete | Refused while |
| --------- | ------------- |
| A tool or tool instance | an agent calls it or runs in it, a toolset includes it, a harness instance provides it, a pipeline stage runs it, or another tool's spec names it |
| A toolset or toolset instance | an agent calls it |
| An agent | a workflow orchestrates it or another agent delegates to it |
| A workflow | never (nothing points at a workflow) |

Drop the reference first (`unlink-tool`, `unlink-toolset`, `undelegate`, `workflows update`, or
delete the referrer), then delete; the order of deleting several is up to you as long as each
referrer goes before what it references. A tool a toolset only picks up through its tag selector
doesn't count: those members are found when an instance is built, so such a tool deletes freely
and just leaves the next instance. Existing toolset and workflow instances are snapshots; they
never block a delete and stay readable after what they snapshotted is deleted.

### Running a workflow
---

`thorctl run workflow` (also spelled `thorctl workflows run`) takes the workflow's name for its latest Ready instance, `<workflow>#r<N>`
for one revision, or an instance id (a workflow id also means its latest instance). A workflow with
no Ready instance fails naming `thorctl instances request workflow NAME`:

```bash
thorctl run workflow malware-triage --sample <SHA256> --wait
thorctl run workflow "malware-triage#r3" --file ./dropper.bin --wait --output ./triage-out
```

The run flags are shared with tool and pipeline runs; see
[Running Tools and Workflows](../users/runs.md).

### Using the API
---

Agents, toolsets, and workflows are entities, so scripts use the entities routes. A create is a multipart
form whose `kind` field is `Agent`, `ToolSet`, or `WorkflowCatalog` (a workflow); the OpenAPI document
`/api/entities/openapi.json` lists the fields (see the [REST API
Reference](../architecture/api.md#rest-api-reference)).

| Task | Route |
| ---- | ----- |
| Create one | `POST /api/entities/` |
| List with filters, or in full | `GET /api/entities/`, `GET /api/entities/details/` |
| Read, change, or delete one | `GET`, `PATCH`, `DELETE /api/entities/{id}` |
| Tag or untag one | `POST`, `DELETE /api/entities/tags/{id}` |
| Ask for a workflow or toolset instance | `POST /api/entities/workflows/{id}/instance`, `POST /api/entities/toolsets/{id}/instance` |
| Read the toolset of a workflow instance | `GET /api/entities/workflows/instances/{id}/toolset` |
| Run a workflow instance | `POST /api/workflows/instances/{id}/runs` |

Common errors: `401` when you lack edit rights or the `Entities` allow flag in a group, `404` when a group,
tool, or linked entity doesn't exist or isn't visible, `409` when the change conflicts with how other
entities use this one (for example removing a group that something linked still needs) or when deleting
something still in use, and `422` when the request has the wrong shape.
