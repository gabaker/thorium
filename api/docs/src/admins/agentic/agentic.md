# Tools, Runs, and Models for Admins

Thorium runs every analysis job as a **tool instance**: an immutable, versioned definition of a
tool. Pipelines name a tool for each stage, and tools and AI workflows can also be run directly
on files and repos (a **run**). This chapter covers what admins need to know to configure and
operate these features. What users and developers do with them is covered in
[Running Tools and Workflows](../../users/runs.md),
[Tools and Tool Instances](../../developers/tools.md), and
[AI Agents, Toolsets, and Workflows](../../developers/agents_workflows.md).

| Page | What it covers |
| ---- | -------------- |
| [The Thorium MCP Server](./mcp.md) | The MCP server at `/api/mcp`, its tools, and the `Host` allow list |
| [Model Endpoints for Admins](./models.md) | The allowed model endpoint list, admin bans, and the admin-only model routes |
| [Scheduling Tools and Runs](./scheduling.md) | How the scaler, agent, and reactor run tool instances and per-run workers |
| [Upgrading to Agentic Workflows](./upgrading.md) | The upgrade steps that convert images, pipelines, and provenance: the drain wait, approvals, the outage, issues, and rollback |

> **Known gaps in this release.** The Web UI rejects edits to the tools of a FileSystem or
> WindowsProcessTree, and its Models page is read only (see
> [The Web UI](../../concepts/concepts.md#the-web-ui) for what it supports). The BareMetal,
> Windows, and Kvm launchers of the reactor have not been tested on live nodes and may not work.
> See [Release Notes](../../release_notes/agentic_workflows.md).

### Configuration
---

These keys in the Thorium config file (`thorium.yml`, see `api/thorium-template.yml`) control the
features in this chapter. Like every other key, each can be overridden with an environment variable
whose path segments are joined by double underscores, for example
`THORIUM__THORIUM__TOOL_ACCESS__MCP_URL`. On a Helm deployment, set them under
`operator.cluster.config.thorium` (see [Configure the Helm Charts](../deploy/helm-configuration.md)).

| Key | Default | Meaning |
| --- | ------- | ------- |
| `thorium.mcp.allowed_hosts` | `[]` | More host names (or `host:port` authorities) the MCP server accepts in the `Host` header. See [The Thorium MCP Server](./mcp.md#the-host-allow-list) |
| `thorium.mcp.allowed_origins` | `[]` | The browser origins (`scheme://host[:port]`) allowed to call the MCP server; empty means the `Origin` header isn't checked |
| `thorium.tool_access.cli` | `["/opt/thorium/thorium-agent", "tool", "run"]` | The command harnesses use to run tools behind Thorium, including its subcommand |
| `thorium.tool_access.mcp_url` | unset | The url of Thorium's MCP server as worker pods see it; unset means the API url given to workers plus `/api/mcp` |
| `thorium.entities.max_tool_schema_bytes` | `65536` | The largest input or output schema an agentic tool may have, in bytes |
| `thorium.events.workflow_graph_delay` | `10` | Seconds a workflow graph event waits before the event handler takes it, so a burst of changes is handled together |
| `thorium.events.max_agents_per_workflow` | `32` | The most agents a workflow may resolve to before its instance is Invalid |
| `thorium.events.max_tools_per_agent` | `128` | The most tools one agent may have in a workflow instance. An agent may list at most twice this many tools in `Calls` edges, and a harness instance may provide at most twice this many native tools (256 by default) |
| `thorium.events.max_crawl_depth` | `8` | The most delegation hops a workflow may have |
| `thorium.events.max_tools_per_toolset` | `64` | The most tools a toolset instance may have |

The `thorium.events` limits are checked when an instance is built: a workflow or toolset instance
that goes over one is Invalid, and its message names the limit. In the Helm chart they are set under
`operator.cluster.config.thorium.events`.

A harness can override `tool_access` for its own runs in its spec (see
[Harnesses](../../developers/harnesses.md)). The Helm chart has no value for `tool_access`; set it
in the cluster config if the defaults don't fit.

The model endpoints users and groups may call are not in the config file. They are a system
setting that admins edit through the API; see [Model Endpoints for Admins](./models.md).

### Backups
---

`thoradm backup` copies user accounts with their model settings and archives model credential
entities with their secrets in plaintext. Protect backups like the credentials themselves; see
[Admin Command Line Tool](../thoradm/thoradm.md).

### Upgrading
---

The upgrade converts existing images to tools and pipeline stages to those tools (see
[Upgrading Thorium](../deploy/upgrades.md#2026-10-v04)). Images are read-only (creating or updating
one returns `410 Gone`), and a pipeline can only run once every stage has a tool. For an image the
upgrade couldn't convert, a developer creates a tool with `thorctl tools create` and sets every
stage of each pipeline that uses it with `thorctl pipelines stage-tool`; see
[Tools and Tool Instances](../../developers/tools.md#moving-a-pipeline-to-tools).
The steps, their approvals, the outage, and the rollback are covered in
[Upgrading to Agentic Workflows](./upgrading.md). The full list of breaking changes is in the [Release Notes](../../release_notes/agentic_workflows.md).
