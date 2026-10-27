# The Thorium MCP Server

Thorium's API serves a [Model Context Protocol](https://modelcontextprotocol.io) (MCP) server at
`/api/mcp`, so models and MCP clients can read Thorium data and run tools with a user's token. On a
Kubernetes deployment one API pod at a time serves it, through the `thorium-mcp` Service (see
[ThoriumCluster and Operator Reference](../deploy/thoriumcluster.md)).

The server speaks the MCP 2026-07-28 revision over Streamable HTTP only (there is no SSE transport).
Clients on that revision call tools without a session; older clients still negotiate a session.

The MCP endpoint is a protocol endpoint, not a REST route: it is not described by any of the OpenAPI
documents the API serves in its Swagger UI (`/api/docs/swagger-ui/`), and the protocol's own tool
listing is the reference for what it offers.

### Authentication
---

Every request needs an `Authorization` header holding a Thorium token, either as
`<scheme> <token>` or as the bare token. Each tool call acts with that token's normal permissions,
so a user only sees and runs what they could through the API. A request without the header is
refused with `Missing authorization header`.

### Tools
---

| Tool | What it does |
| ---- | ------------ |
| `get_sample` | Get basic info about a file by its SHA256 |
| `get_sample_results` | Get the tool results for a file |
| `list_sample_result_file_paths` | List the result files a tool produced for a file |
| `get_sample_result_file` | Get one result file a tool produced for a file |
| `list_images` | List the images in a group |
| `list_pipelines` | List the pipelines in a group |
| `start_tree` | Crawl the data related to a file, repo, tag, or entity |
| `run_tool` | Start a tool run of a tool instance on files, mapping the call's `input` onto the tool's job; returns the run's reaction id and group as soon as it exists |
| `get_tool_run` | Get a tool run's status without waiting, with a `retry_after_secs` hint while it runs |
| `get_tool_run_results` | Get a finished tool run's newest result and result files on each of its inputs; refuses runs that haven't finished or that failed |

Every tool has a title and annotations saying whether it is read-only, destructive, idempotent, or
reaches outside Thorium. JSON results carry `structuredContent` plus one text block holding the same
JSON, since some clients pass only text to the model. None of the tools hold a request open while a
run works; models poll `get_tool_run` at the pace its `retry_after_secs` hint gives (1 second,
doubling up to 1 minute from the run's start).

#### Run-scoped tokens

Inside a workflow run, a harness calls the server with the run's
[run-scoped token](../../users/runs.md#run-scoped-tokens). For such a token, `tools/list` shows
only the MCP tools the run's own tools are backed by, each described as the run's tool describes it
(title, description, annotations, output schema, and icons), plus the three tool run tools when the
run has a container-backed tool. Calls are not narrowed by that listing: a tool left out still
answers within the token's normal limits. When a run's tool declares an output schema, a result that
doesn't match it is returned as an error naming the mismatch.

### The Host allow list
---

The server refuses any request whose `Host` header it doesn't know with `403 Forbidden: Host header
is not allowed`. This stops pages on other sites from reaching the server through DNS rebinding. It
always accepts:

- loopback: `localhost`, `127.0.0.1`, and `::1`
- every name of the in-cluster `thorium-api` and `thorium-mcp` Services in `thorium.namespace`:
  `<svc>`, `<svc>.<namespace>`, `<svc>.<namespace>.svc`, and `<svc>.<namespace>.svc.cluster.local`
  (with or without a trailing dot)
- the hosts of the API urls in the config: `thorium.tool_access.mcp_url`,
  `thorium.auth.oauth.redirect_base`, `thorium.auth.email.base_url`, and each
  `thorium.scaler.k8s.clusters.<cluster>.api_url`
- every entry of `thorium.mcp.allowed_hosts`

Hosts are compared without regard to case. An entry without a port matches any port; an entry with a
port matches only that port. The list is never empty. The API logs the hosts it allows when it
starts.

**Action for admins:** if MCP clients reach Thorium by a host that isn't in that list (most often
the ingress host), add it:

```yaml
thorium:
  mcp:
    allowed_hosts:
      - "thorium.example.com"
```

The Helm chart does this for you when `operator.ingress.host` is set: it renders that host into
`thorium.mcp.allowed_hosts`. With an empty ingress host, set
`operator.cluster.config.thorium.mcp.allowed_hosts` yourself (see
[Configure the Helm Charts](../deploy/helm-configuration.md)). Clients that reached the server by
any other name before this release now get `403`.

### Origin checks
---

`thorium.mcp.allowed_origins` turns on `Origin` checks for browser clients. When it lists any
origin (`scheme://host[:port]`, with `:*` for any port), a request whose `Origin` header doesn't
match is refused with `403`. Requests without an `Origin` header are always accepted. The check is
off when the list is empty, which is the default.
