# Concepts

Thorium runs analysis tools on files and repos, and since the Agentic Workflows release it can also
run AI workflows. This page names the pieces and says where each is covered. The guides use
[Thorctl](../getting_started/thorctl.md) for their examples and name the API route behind each
command for scripts. The Web UI is being rebuilt, so these guides do not teach it; its instructions
will be added with the new interface.

### The pieces
---

| Concept | What it is | Made and changed with | Guide |
| ------- | ---------- | --------------------- | ----- |
| Group | The unit of sharing and permissions. Every file, repo, tool, workflow, and run belongs to one or more groups, and members hold a role in each | `thorctl groups` | [Roles and Permissions](../getting_started/roles_permissions.md) |
| Tool | A named, versioned thing Thorium can run or a model can call. It has one of three kinds: `Image` (a workload), `Agentic` (a function a model can call), or `Harness` (the program that runs an agent's loop) | `thorctl tools` | [Tools and Tool Instances](../developers/tools.md) |
| Tool instance | One immutable version of a tool, named `<tool>@<version>`. Everything Thorium runs is a tool instance | `thorctl tools publish`, `thorctl instances` | [Tools and Tool Instances](../developers/tools.md) |
| Toolset | A named set of agentic tools that agents attach as one unit | `thorctl toolsets` | [AI Agents, Toolsets, and Workflows](../developers/agents_workflows.md#toolsets) |
| AI agent | A configured agent: prompts, the models it accepts, its tools, and the agents it may delegate to. It runs in a harness | `thorctl agents` | [AI Agents, Toolsets, and Workflows](../developers/agents_workflows.md#ai-agents) |
| Harness | The program that runs an agent's model and tool loop inside a workflow run | `thorctl tools` (kind `Harness`) | [Harnesses](../developers/harnesses.md) |
| Workflow | The editable root that wires agents together. Building one creates a constant, numbered workflow instance (`<workflow>#r<revision>`) that runs | `thorctl workflows`, `thorctl instances` | [AI Agents, Toolsets, and Workflows](../developers/agents_workflows.md#workflows) |
| Run | One execution of a tool, workflow, or pipeline on files and repos. Every run is a reaction | `thorctl run tool\|workflow\|pipeline`, `runs` | [Running Tools and Workflows](../users/runs.md) |
| Model endpoint, credential, profile | Where a run's models come from and how they are called | `thorctl models` | [Models](../users/models.md) |
| Result and provenance | What a run produced, stored on each input under the run's result key, and the exact versions the run used | `thorctl results`, `thorctl runs describe` | [Results and Provenance](../users/viewing_results.md) |
| Ban and notification | Why a model, endpoint, pipeline, or legacy image cannot be used right now | `thorctl models bans`, `thorctl pipelines notifications` | [Bans](../developers/bans.md) |
| Toolbox | A directory (or bundle) of tools, toolsets, agents, workflows, and pipelines that moves between instances | `thorctl toolbox` | [Toolboxes](../developers/toolbox.md) |
| Image, pipeline | An image is the older way to describe a tool and is now read-only; a pipeline is a static chain of stages, and each stage runs a tool | `thorctl images`, `thorctl pipelines` | [Legacy Images and Pipelines](../developers/images.md) |

### What to use for what
---

| I want to... | Use | Read |
| ------------ | --- | ---- |
| Add a command line tool that Thorium runs on files | An `Image` tool | [Tools and Tool Instances](../developers/tools.md#an-image-tool) |
| Give a model a function it can call | An `Agentic` tool, optionally in a toolset | [Tools and Tool Instances](../developers/tools.md), [Toolsets](../developers/agents_workflows.md#toolsets) |
| Let a model plan and call tools on my files | An agent in a workflow | [AI Agents, Toolsets, and Workflows](../developers/agents_workflows.md) |
| Run one tool on some files | `thorctl run tool` | [Running Tools and Workflows](../users/runs.md#tool-runs) |
| Run a workflow on some files | `thorctl run workflow` | [Running Tools and Workflows](../users/runs.md#workflow-runs) |
| Run a chain of tools in order | A pipeline, with `thorctl run pipeline` | [Pipeline runs](../users/runs.md#pipeline-runs), [Building Pipelines](../developers/build_pipelines.md) |
| Run a pipeline on every file with a tag | `thorctl reactions create` | [Spawning Reactions](../users/spawning_reactions.md) |
| Start one run per file in a list, follow or cancel them together | `thorctl run <kind> <name> --one-per-input --batch NAME` | [Batch runs](../users/runs.md#batch-runs) |
| Choose which models a run may use | Endpoints, profiles, and bans | [Models](../users/models.md) |
| See what a run did and which versions it used | `thorctl runs describe`, `runs logs` | [Following runs](../users/runs.md#following-runs) |
| Copy tools between instances or keep them in git | A toolbox | [Toolboxes](../developers/toolbox.md) |
| Move an existing instance's images and pipelines to tools | The upgrade conversion, or `thorctl toolbox convert` | [Upgrading to Agentic Workflows](../admins/agentic/upgrading.md), [Upgrading: Changes to Tool Commands](../developers/tool_cli_changes.md) |

### How the guide is organized
---

1. [Getting Started](../getting_started/getting_started.md): accounts, groups, and installing Thorctl.
2. This page, then [Tools, Toolsets, and Agents](../developers/tools.md) and
   [Workflows](../developers/agents_workflows.md#workflows): defining what can run.
3. [Running Tools and Workflows](../users/runs.md): single runs, following them, cancelling them,
   and reading their logs and results.
4. [Models](../users/models.md) and [Bans](../developers/bans.md): which models runs may use and
   why something is refused.
5. [Results and Provenance](../users/viewing_results.md): reading what runs produced.
6. [Legacy Images and Pipelines](../developers/images.md): the settings an Image tool still holds,
   pipelines, and how images convert to tools.
7. [Toolboxes](../developers/toolbox.md): import, export, and conversion.
8. [Upgrading](../developers/tool_cli_changes.md): what changed for command line users, and the
   [admin upgrade](../admins/agentic/upgrading.md).

Pages that describe a capability share a shape: what it is, when to use it, the Thorctl commands to
create, change, run, inspect, and delete it, the API routes for scripts, who may do it, and its
limits and common errors. The API is described route by route in the
[REST API Reference](../architecture/api.md#rest-api-reference) served by your instance.
