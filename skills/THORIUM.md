---
name: thorium
description: Guide for AI agents to interact with Thorium, a highly scalable file analysis and data generation framework. Covers instance discovery, API routes, thorctl CLI, documentation, and workflow references.
---

# Thorium

Thorium is a highly scalable, distributed malware analysis and data generation framework built for cyber incident response, triage, and file analysis. It provides safe ingestion/storage of data, automated analysis in sandboxed environments, and easy access to results and metadata.

Source code: `github.com/cisagov/thorium`

## Step 1: Discover the Thorium Instance

Before doing anything else, determine the Thorium instance URL. Follow these steps in order:

1. **Check for a running local instance:**
   ```sh
   curl -sf -o /dev/null -w '%{http_code}' http://localhost:8080/api/health
   ```
   If the response is `204`, a local instance is running. Set `THORIUM_URL=http://localhost:8080`.

2. **If not reachable, check if minithor is available and try to expose:**
   ```sh
   command -v minithor || test -x /usr/bin/minithor
   ```
   `minithor` is normally on your `PATH`; some environments instead place it at
   `/usr/bin/minithor`, so fall back to that path if it isn't found. If minithor exists, run:
   ```sh
   minithor expose
   ```
   Wait a few seconds, then re-check health:
   ```sh
   curl -sf -o /dev/null -w '%{http_code}' http://localhost:8080/api/health
   ```
   If `204`, set `THORIUM_URL=http://localhost:8080`.

3. **If still not reachable (or minithor is not installed):**
   Ask the user: "No local Thorium instance detected. Please provide the URL to your Thorium instance (e.g., `https://thorium.example.com`)."
   Set `THORIUM_URL` to the user-provided URL.

Use `THORIUM_URL` as the base for all URLs below.

> **Sandboxed agents (e.g. Claude Code):** If you are a coding agent running commands in a network
> sandbox, connections to the Thorium API — including a local minithor at `http://localhost:8080`
> and any `minithor expose` port-forward — may be blocked by default. If health checks, `thorctl`,
> `curl`, or API calls fail with connection-refused/permission/"operation not permitted" errors that
> don't match a real outage, the request likely needs to reach Thorium **outside the sandbox**:
> allowlist the Thorium host/port (e.g. `localhost:8080`, the backing-service ports from
> `minithor expose --dev`, and any remote `THORIUM_URL`) or run that command with the sandbox
> disabled. Use your agent's sandbox controls to permit the connection before assuming the instance
> is down.

## Step 2: Resource Directory

These are the key resources available on a Thorium instance:

| Resource | URL | Description |
|----------|-----|-------------|
| Web UI | `{THORIUM_URL}` | Browser interface for file browsing, uploading, reactions, search, and results, and for tools, toolsets, agents, workflows, models (read only), launching runs, and following them |
| User Docs | `{THORIUM_URL}/api/docs/user/index.html` | Guides for uploading files, viewing results, spawning reactions, search, tagging. If the API is offline, use `https://cisagov.github.io/thorium/` as a fallback |
| Developer Docs | `{THORIUM_URL}/api/docs/dev/thorium/index.html` | Rust API documentation (rustdoc) for Thorium internals |
| API Spec (Swagger) | `{THORIUM_URL}/api/docs/swagger-ui/` | Interactive OpenAPI spec — full details on every endpoint, request/response schemas |
| MCP Server | `{THORIUM_URL}/api/mcp` | Model Context Protocol endpoint for AI tool use (requires Authorization header) |

## Step 3: API Authentication

The Thorium API is a REST API served under `{THORIUM_URL}/api/`. Connecting to it is the same for
a **local** instance (e.g. minithor at `http://localhost:8080`, see Step 1) and a **remote** one —
only the base URL differs. To explore every endpoint and its request/response schema interactively,
open the Swagger UI at `{THORIUM_URL}/api/docs/swagger-ui/`.

All API routes (except `/api/health`, `/api/banner`, and `/api/`) require authentication via the `Authorization` header. Two methods are supported:

### Method 1: Username/Password (Basic Auth)

Authenticate with HTTP Basic auth to obtain a token. Base64-encode `username:password` and send:

```
Authorization: Basic <base64(username:password)>
```

Call `POST /api/users/auth` with this header to receive a JSON response containing a `token` and its `expires` datetime.

### Method 2: Token Auth (Preferred)

Once you have a token (from Basic auth, `thorctl login`, or the config file at `~/.thorium/config.yml`), base64-encode the token and send:

```
Authorization: Token <base64(token)>
```

or equivalently:

```
Authorization: Bearer <base64(token)>
```

Token auth is used for all subsequent API requests. Tokens expire and must be refreshed by re-authenticating.

### Using the Rust Client

The Thorium Rust client (`thorium::Thorium`) handles auth automatically:

```rust
// With username/password:
let client = Thorium::build("http://localhost:8080")
    .basic_auth("user", "password")
    .build().await?;

// With an existing token:
let client = Thorium::build("http://localhost:8080")
    .token("my-token")
    .build().await?;

// From a keys file (contains api, username/password or token):
let client = Thorium::build("http://localhost:8080")
    .from_keys("/path/to/keys.yml")?
    .build().await?;

// From a thorctl config (CtlConf; the path is not tilde-expanded):
let ctl_conf = thorium::CtlConf::from_path("/home/me/.thorium/config.yml")?;
let client = Thorium::build(&ctl_conf.keys.api)
    .from_ctl_conf(ctl_conf)?
    .build().await?;
```

### Using curl

```sh
# Authenticate and get a token:
curl -X POST -u "username:password" {THORIUM_URL}/api/users/auth

# Use the token for subsequent requests (base64-encode the token):
curl -H "Authorization: Token $(echo -n 'YOUR_TOKEN' | base64)" {THORIUM_URL}/api/version
```

## Step 4: API Route Reference

All API routes are under `/api/`.

### Core Routes

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/` | Identify this API as Thorium |
| `GET` | `/api/health` | Health check — `204` healthy, `503` unhealthy |
| `GET` | `/api/version` | Current Thorium version (requires auth) |
| `GET` | `/api/banner` | Login banner message |

### Domain Routes

See Swagger UI (`{THORIUM_URL}/api/docs/swagger-ui/`) for complete endpoint details including request/response schemas. Each area is also served as an OpenAPI document at `/api/<area>/openapi.json` (for example `/api/entities/openapi.json`); see `{THORIUM_URL}/api/docs/user/architecture/api.html#rest-api-reference` for the area names.

| Area | Route Prefix | Key Operations |
|------|-------------|----------------|
| Files | `/api/files/` | Upload, download (CaRT or encrypted ZIP), list, count, update, delete, tag, comment, list associations, view/upload results |
| Groups | `/api/groups/` | Create, list, update, delete groups; LDAP sync; group stats |
| Images | `/api/images/` | List, get, delete images; manage image notifications. Create, update, and image bans return `410 Gone`: Thorium runs Tools instead |
| Tool and workflow runs | `/api/tools/instances/{id}/runs`, `/api/workflows/instances/{id}/runs` | Run a tool instance or a Ready workflow instance on files/repos; retry a failed workflow run (`.../runs/{reaction}/retry`) |
| Pipelines | `/api/pipelines/` | Create, list, update, delete tool chains (every stage needs a tool in `stage_tools`); manage notifications and bans |
| Reactions | `/api/reactions/` | Create, list, get, update, delete reactions; view/set logs; handle lifecycle (proceed/fail); manage cache and sub-reactions |
| Results | `/api/files/results/:sha256` | Get and upload tool results and result files for a sample |
| Repos | `/api/repos/` | Ingest, list, download, tag repos; list commitishes; list associations; view/upload results |
| Search | `/api/search/` | Full-text search across sample results and tags (defaults to samples; repo indexes also available) |
| Streams | `/api/streams/` | Query stream depth (object counts over time ranges) and map data |
| Users | `/api/users/` | Create, list, update, delete users; authenticate; email verification; LDAP sync |
| System | `/api/system/` | System init, settings, stats, backup/restore, cache reset, node/worker management |
| Trees | `/api/trees/` | Build and retrieve relationship trees from samples, repos, or entities |
| Entities | `/api/entities/` | Create, list, update, delete entities (including Tools, Tool Instances, Agents, ToolSets, Workflows, and model endpoints, profiles, and credentials); manage entity tags and images; list associations; `/api/entities/tools/{id}/instances` lists a tool's versions |
| Associations | `/api/associations/` | Create and delete associations between files, repos, and entities |
| Binaries | `/api/binaries/` | Download thorctl and other Thorium binaries |
| Events | `/api/events/` | Internal event queue operations (pop, clear, reset) |
| Jobs | `/api/jobs/` | Internal job lifecycle (claim, proceed, error, sleep, checkpoint, bulk reset) |
| Network Policies | `/api/network-policies/` | Create, list, update, delete network policies; view group defaults |

### MCP Tools

The MCP server at `/api/mcp` exposes these tools (requires `Authorization` header):

- `get_sample` — Get basic info about a specific sample/file by SHA256
- `get_sample_results` — Get tool results for a specific sample/file by SHA256
- `list_sample_result_file_paths` — List all available result files for a tool for a specific sample/file
- `get_sample_result_file` — Get a specific result file from a tool for a specific sample
- `list_images` — List the images (analysis tools) in a group
- `list_pipelines` — List the pipelines in a group
- `start_tree` — Find data related to a sample, repo, tag, or entity by crawling data in a tree structure
- `run_tool` — Start a tool run of a tool instance (returns the reaction id and group at once)
- `get_tool_run` — Get a tool run's status, with a `retry_after_secs` polling hint
- `get_tool_run_results` — Get a finished tool run's results on each of its inputs

The server only answers requests whose `Host` header is loopback, an in-cluster `thorium-api` or
`thorium-mcp` service name, an API url host from the config, or listed in
`thorium.mcp.allowed_hosts`; any other host gets `403` (see
`{THORIUM_URL}/api/docs/user/admins/agentic/mcp.html`).

## Step 5: thorctl CLI (User/Developer Tool)

`thorctl` is the command-line tool for large-scale Thorium operations (file/repo upload and
download, tool/workflow/run/model/pipeline/toolbox management, running reactions, tagging, search, and more). It
talks to the same REST API as the Web UI and works against any reachable instance — local
(minithor) or remote.

### Setup (get started)

1. **Obtain the binary.** `minithor deploy` installs `thorctl` automatically. Otherwise check for
   it and install from the instance if missing:

   ```sh
   which thorctl  # already installed?
   # install (or update) directly from the target instance:
   curl {THORIUM_URL}/api/binaries/install-thorctl.sh | bash -s -- {THORIUM_URL}
   ```

   The script downloads the build matching the instance; `thorctl update` updates it later.

2. **Authenticate / choose a cluster.** `login` points thorctl at an instance and stores a token:

   ```sh
   thorctl login {THORIUM_URL}                 # prompts for username + password interactively
   thorctl login {THORIUM_URL} --user <name>   # non-interactive: add --password <pass> for CI
   thorctl login {THORIUM_URL} --clear-settings # reset config and start fresh for this instance
   ```

   For self-signed/dev certs, `login` accepts `--invalid-certs`, `--invalid-hostnames`, and
   `--certificate-authorites <path>`.

3. **Config file.** Credentials and settings are written to `~/.thorium/config.yml`. Point any
   command at a different config (e.g. a second cluster) with the global `--config <path>` flag, or
   supply a standalone keys file with `--keys <path>`. `thorctl config` edits these settings.

See `{THORIUM_URL}/api/docs/user/getting_started/thorctl.html` for per-OS install detail, and
`{THORIUM_URL}/api/docs/user/getting_started/login.html` /
`{THORIUM_URL}/api/docs/user/getting_started/registration.html` for account/login detail.

### Key Subcommands

| Command | Description |
|---------|-------------|
| `thorctl login <url>` | Authenticate with a Thorium instance (see Setup above) |
| `thorctl clusters status` / `workers` | View cluster node resource status and per-worker info |
| `thorctl groups` | List and describe groups you belong to |
| `thorctl files` | Upload, download, get, count, describe, and delete files; `files upload -g <group> <paths> [-p <group>/<pipeline>] [--workflows <name>] [--tools <name>] [--batch <name>] [--wait\|--watch]` starts pipelines (one bulk request per 100 reactions), workflow runs, and tool runs on each newly uploaded file and follows them (exit 0 all good, 1 any upload or run failed, 3 timed out; `-o json` prints a line per file and per run; `--dry-run` previews) |
| `thorctl images` | Get, describe, export, and delete images (images are read-only: `edit`, `import`, and image bans are refused with a pointer to tools); image notifications |
| `thorctl pipelines` | Get, describe, create, update, edit, run, delete, import/export pipelines; set a stage's tool (`stage-tool`); manage pipeline bans and notifications |
| `thorctl tools` | List, describe, create, publish, edit, update, spec, snapshot, deprecate, and delete tools; `versions` lists a tool's versions and `harness-env`/`harness-models` change a harness's model settings; `tools run` (or `run tool`) runs a tool instance (`<tool>@<version>`) and prints one JSON object; `tools runs status/results/logs` |
| `thorctl instances` | List, describe, and diff tool, toolset, and workflow instances; `request` asks for a new toolset or workflow instance now |
| `thorctl agents` / `toolsets` / `workflows` | Build AI agents, toolsets, and workflows; `workflows run` (or `run workflow`) runs a workflow instance (`<workflow>#r<revision>`) |
| `thorctl runs` | List runs (`get --kind tool\|workflow\|pipeline --batch --status`), describe any run (models, plan, provenance), read its logs, retry a failed workflow run, cancel a run or a whole batch (`cancel --batch`, with `--dry-run`) |
| `thorctl models` | Model endpoints, catalogs, profiles, credentials, bans, refresh, and your own model settings |
| `thorctl reactions` | Create, get, describe, delete reactions; retrieve reaction logs; `update` adds or removes a reaction's tags and `fail` fails one that hasn't finished |
| `thorctl results` | Download results for files/repos and upload new results; `results show <reaction>` shows a run's results |
| `thorctl tags` | Get, add, and delete tags on files and repos |
| `thorctl associations` | Create, delete, and list associations between files, repos, and entities |
| `thorctl trees` | Delete the descendant entities under a set of initial tree nodes |
| `thorctl repos` | Ingest, update, get, describe, download, compile repos; list commits and contributors |
| `thorctl network-policies` | Get, describe, create, update, delete, and verify network policies; view group defaults |
| `thorctl cart` | Cart (encrypt/neuter) files locally |
| `thorctl uncart` | Uncart files locally |
| `thorctl run` | `run tool\|workflow\|pipeline <NAME>` starts a run; `--one-per-input --batch <NAME> --workers N` fans a list (`--sample-list -` reads stdin) into one run per input and `--wait` follows the batch (exit 0 all completed, 1 any failed, 3 timed out; `-o json` prints JSON lines); `--dry-run` previews. The legacy `run <PIPELINE> <SHA256>` still creates one reaction and shows its output |
| `thorctl toolbox` | `validate`, `list`, `convert`, `import`, `update`, `diff`, `export`, `remove`, `init`, `build`, and `build-images` for toolboxes: portable collections of tools, toolsets, agents, workflows, and pipelines (see Step 10 and `{THORIUM_URL}/api/docs/user/developers/toolbox.html`) |
| `thorctl config` | Modify thorctl configuration settings |
| `thorctl update` | Update thorctl binary |

Use `thorctl -h` or `thorctl <subcommand> -h` for detailed help on any command.

> Building, integrating, or distributing your own tools? See **Step 10** for the developer
> workflow (`tools` commands for one tool, `toolbox` for collections).

## Step 6: thoradm CLI (Admin Tool)

`thoradm` is the admin-only CLI for Thorium infrastructure management. It requires both a thorctl config (for user auth) and a cluster config (`thorium.yml`) containing database credentials.

Check if it is available:

```sh
which thoradm
```

### Getting the Cluster Config

thoradm requires a `thorium.yml` cluster config file containing database credentials (Elastic, Redis, Scylla, etc.). There are two ways to obtain it:

**Option 1: minithor (local development)**

```sh
minithor get-config
```

This writes the config to `~/thorium.yml`.

> The raw config points at in-cluster `*.svc.cluster.local` addresses. If you are running
> `thoradm` (or any Thorium binary) **from your host** against a minithor cluster, the
> databases are only reachable through `minithor expose --dev`; use
> `minithor get-config --local` to get a config rewritten for those local ports. See
> "Step 7 → Testing Locally" for details (Scylla in particular needs special handling).

**Option 2: kubectl (external Kubernetes cluster)**

Extract the config directly from the cluster secret:

```sh
kubectl get secret thorium -n thorium \
    -o go-template='{{index .data "thorium.yml" | base64decode}}' \
    > thorium.yml
```

By default, thoradm looks for `thorium.yml` in the current directory. Override with:

```sh
thoradm -c <PATH-TO-THORIUM.YML>
# or: thoradm --cluster-conf <PATH-TO-THORIUM.YML>
```

### Key Subcommands

| Command | Description |
|---------|-------------|
| `thoradm backup new` | Take a full backup of Thorium data (Redis, S3, tags, metadata) |
| `thoradm backup restore --backup <path>` | Restore a backup (overwrites all current data) |
| `thoradm backup scrub --backup <path>` | Verify backup integrity via checksums |
| `thoradm settings get` | View current system settings (resource limits, fairshare, host paths) |
| `thoradm settings update` | Update system settings (reserved CPU/memory/storage, fairshare, host path whitelist) |
| `thoradm settings reset` | Reset all system settings to defaults |
| `thoradm settings scan` | Run a manual consistency scan across all Thorium data |
| `thoradm provision node` | Provision a Kubernetes node for Thorium job scheduling |
| `thoradm census new` | Run a data consistency census (all, tags, files, repos, commitishes) |

Use `thoradm -h` or `thoradm <subcommand> -h` for detailed help on any command.

For full thoradm documentation, see the User Docs at `{THORIUM_URL}/api/docs/user/admins/thoradm/thoradm.html`.

`thoradm migrate list` and `thoradm migrate plan --from <revision>` print the upgrade revisions
and the steps between them without needing a cluster config; inside a cluster, run
`kubectl -n thorium exec deploy/api -- /app/thoradm migrate list`. `thoradm backup` covers Redis,
Scylla, and S3; Elasticsearch isn't backed up because the search streamer rebuilds it from Scylla.

### Deployment docs

The admin deployment guide is served at `{THORIUM_URL}/api/docs/user/admins/deploy/deploy.html`
("Deploying Thorium"). Its pages under `{THORIUM_URL}/api/docs/user/admins/deploy/` include
`deploy-helm.html` (install with Helm), `helm-configuration.html` (chart values, credentials,
external services), `operate.html` (status, backups, uninstalling), `upgrades.html` (revisions,
target revisions, approvals), `convert-to-helm.html` (pre-Helm deployments), `deploy-thorium.html`
(the operator without Helm), `minithor.html`, `megathor.html`, `thoriumcluster.html` (the
`ThoriumCluster` reference), and `troubleshooting.html`. In a checkout they are
`api/docs/src/admins/deploy/*.md`.

## Step 7: minithor (Local Development)

`minithor` stands up a **Thorium stack on Minikube** (single-node by default, optionally multi-node)
— a self-contained local instance for developing features against, and for building/testing/curating
tools. It is not highly available and is for development/testing only (or small offline fly-away
kits), not production. A deploy brings up all backing services (Redis, Elasticsearch, ScyllaDB,
SeaweedFS/S3, Postgres, Quickwit, Jaeger), the Thorium operator and `ThoriumCluster`, a default
admin user, a `static` group, an `allow-all` network policy, and (optionally) an in-cluster
container registry; it also installs `thorctl` and imports the default toolbox. Backing-service
passwords are generated on the first deploy and kept in the `thorium-credentials` Secret, so
redeploys reuse them. minithor sets the chart's `operator.cluster.upgrade.autoTargetDev`, so a
redeploy with a newer chart also runs any new upgrade revision (a step that needs an approval makes
`deploy` fail with the step's message; see "Upgrading Thorium" in the deploy docs below).

Requirements: a container runtime (podman or docker; `kvm2` also works on Linux), `sudo` rights,
and a beefy host (16+ GiB RAM, 8+ CPUs, 100+ GiB disk by default; `--cpus`/`--memory` are totals
for the whole cluster, however many nodes it has). It picks the best available driver
(podman > docker > kvm2).

Not all Thorium deployments use minithor. Check if it is available before using any of the commands below:

```sh
command -v minithor || test -x /usr/bin/minithor
```

`minithor` is normally on your `PATH`; some environments instead place it at `/usr/bin/minithor`,
so check that path if it isn't found. If neither resolves, this environment uses a different
deployment method and these commands do not apply.

### Stand it up

```sh
minithor minikube install   # first time only: install minikube + start the k8s cluster
minithor deploy             # deploy all services + create admin user (default: test / INSECURE_DEV_PASSWORD)
minithor expose             # port-forward the API to http://localhost:8080
```

`deploy` accepts `--user`/`--password`/`--rand-password` to set the admin credentials,
`--registry`/`--registry-user <name>` to deploy the container registry, and `--values <file>` /
`--set <key=value>` for any other thorium chart value (`minithor deploy --help` lists every
option). Only the generated credentials survive a redeploy: each `deploy` renders the chart values
from its own flags, so repeat the flags you deployed with. `minithor credentials` prints the admin
login (`--all` adds every backend credential). After `minithor stop` or a reboot, resume with
`minithor start` then `minithor expose` again. A cluster runs one Thorium deployment, so
`deploy` refuses a second namespace prefix and a pre-Helm deployment (made by an older minithor or
megathor), which must be converted first ("Converting a Pre-Helm Deployment" in the deploy docs).

`minikube install` also accepts `--cpus <n>`/`--memory <GiB>` (cluster totals split evenly across
the nodes, default 8/16; `--node-cpus`/`--node-memory` size each node directly and conflict with
them), `--certs <path>` (a proxy CA `.crt` file or directory to trust on every node — required
behind a TLS-intercepting proxy or nodes fail to pull images with `ImagePullBackOff`), and
`--force-node-config` (rewrite the nodes' CA/proxy config even if already present).

### Multi-node clusters

To test larger deployments, create the cluster with several nodes; `deploy` then schedules Thorium jobs
across all of them:

```sh
minithor minikube install --nodes 3 --cpus 15 --memory 48   # totals: 5 CPUs / 16 GiB per node
minithor deploy
```

- Nodes are named `<profile>`, `<profile>-m02`, `<profile>-m03`, ... (`minikube`, `minikube-m02`, ... by default).
- The thorium chart leaves the ThoriumCluster's scaler node list (`operator.cluster.scaler.k8s.nodes`)
  empty, which the operator treats as every node: it labels (`thorium=enabled`), provisions, and
  registers each one, and the scaler only schedules on labeled, registered nodes. To restrict jobs to
  specific nodes, pass a list with `--set`/`--values`; the names must match the real node names exactly.
- `--cpus`/`--memory` are cluster totals (split evenly, rounded down); `--node-cpus`/`--node-memory`
  set per-node sizes instead. Each node needs more than 2 CPUs and 2 GiB of memory (the scaler
  reserves that much per node), and install refuses a split that leaves less.
- Per-node setup runs on every node: the proxy/CA settings during `install` and `start`, the kernel
  params during `deploy` and `start`, and the registry `/etc/hosts` entry during `deploy --registry`
  and `start`.
- Verify: `minikube kubectl -- get nodes -L thorium` shows `enabled` on every node,
  `GET /api/system/nodes/details/` lists each node as `Healthy`, and the scaler logs per-node
  allocatable resources (`minikube kubectl -- logs -n thorium deploy/scaler | grep 'node='`).
- With the docker driver, nodes report the **host's** CPU/memory to Kubernetes (not the per-node
  CPU/memory limits), so the scaler over-estimates capacity.

### Separate clusters (profiles)

`minithor --profile <name> <command>` (or `MINITHOR_PROFILE=<name>`/`MINIKUBE_PROFILE=<name>`) targets a separate minikube
profile, so a test cluster (e.g. a multi-node one) can run beside the default `minikube` cluster.
It applies to every command; `minithor --profile <name> minikube delete --confirm` deletes only that
profile's cluster. minithor always passes the profile's kubectl context, but minikube switches the
current context to the profile it last started, so for your own kubectl commands switch back with
`kubectl config use-context minikube`. A second profile writes `~/thorium-<name>.yml` from
`get-config` and logs thorctl in under `~/.thorium/config-<name>.yml` (use `thorctl --config`).

### Reach the running instance

Once exposed, `THORIUM_URL=http://localhost:8080` (Step 1). Log in via the UI or
`thorctl login http://localhost:8080` with the deploy credentials, or hit the API directly (Step 3).
The instance serves its own docs at the same paths under `{THORIUM_URL}/api/docs/...` (Step 2) —
the user docs, Swagger UI, and rustdoc are all reachable locally at `http://localhost:8080/api/docs/...`,
so every doc link in this guide resolves against the running minithor instance, not just a remote one.
`minithor expose --dev` also forwards the backing-service ports (Elastic, Kibana, Redis, SeaweedFS,
Scylla), and `minithor get-config` writes the cluster config (DB credentials) to `~/thorium.yml` —
needed by `thoradm` (Step 6).

| Command | Description |
|---------|-------------|
| `minithor minikube install` | Install minikube and start a Kubernetes cluster |
| `minithor minikube install --nodes <n>` | Create an `n`-node cluster (`--cpus`/`--memory` are cluster totals split across nodes; `--node-cpus`/`--node-memory` size each node); `deploy` schedules jobs on every node |
| `minithor deploy` | Deploy all Thorium services and backing infrastructure (add `--registry` for an in-cluster registry) |
| `minithor expose` | Port-forward the Thorium API to localhost:8080 (and the registry to localhost:5000 when one was deployed) |
| `minithor expose --port <port>` | Port-forward the API to a custom local port |
| `minithor expose --port-offset <n>` | Shift every local port by `n` (to expose a second profile beside the first) |
| `minithor credentials [--all]` | Print the admin login (and every generated backend credential) |
| `minithor expose --dev` | Also forward database ports (Elastic, Kibana, Redis, SeaweedFS, Scylla). See "Testing locally" below — Scylla requires special handling |
| `minithor expose --status` | Show which port-forwards are running |
| `minithor expose --stop` | Stop all port-forwards **and remove any loopback aliases** created for Scylla |
| `minithor start` | Start a previously stopped cluster |
| `minithor stop` | Stop the cluster (preserves state) |
| `minithor get-config` | Extract the raw in-cluster config to ~/thorium.yml |
| `minithor get-config --local` | Extract the config **rewritten for local host access** via the exposed ports, to ~/thorium.local.yml (see "Testing locally") |
| `minithor cleanup --confirm` | Remove Thorium, its namespaces and data for a fresh deploy (also stops its port-forwards and removes the Scylla loopback aliases); `--operators` also removes the cluster-wide operators and their CRDs |
| `minithor --namespace-prefix <prefix> <command>` | Run `deploy`/`credentials`/`expose`/`get-config`/`cleanup` against a Thorium deployment whose namespaces are prefixed `<prefix>-` (`<prefix>-thorium`, `<prefix>-redis`, ...); pass the same prefix to every command. A cluster runs one Thorium deployment; use a second `--profile` (and `expose --port-offset <n>`) to run two |
| `minithor minikube delete --confirm` | Delete the selected profile's cluster (also stops port-forwards and removes loopback aliases); `--purge` also removes `~/.minikube`, `~/.kube`, and the binaries once no other profile remains |
| `minithor --profile <name> <command>` | Run any command against a separate minikube profile (cluster) instead of `minikube` |

### Testing Locally: Connecting a Local Build to the Cluster Databases

To run a local build of the API (or other Thorium services/tools) against the databases
running in the minikube cluster, you need two things: the database ports forwarded to your
host, and a config whose connection fields point at those local ports.

**1. Forward the database ports:**

```sh
minithor expose --dev
```

This forwards, in addition to the API (`localhost:8080`) and the registry (`localhost:5000`,
when one was deployed):

| Service | Local address | Notes |
|---------|---------------|-------|
| Elasticsearch | `https://localhost:9200` | self-signed cert — clients must skip cert validation (see below) |
| Kibana | `https://localhost:5601` | |
| Redis | `localhost:6379` | |
| SeaweedFS (S3) | `http://localhost:8333` | |
| Scylla (CQL) | `<advertised-cluster-ip>:9042` | **not** `localhost` — see "How Scylla is exposed" |

**2. Generate a host-ready config:**

```sh
minithor get-config --local      # writes ~/thorium.local.yml
```

`--local` rewrites the in-cluster config so a host process can connect through the exposed
ports. It changes: Elastic/Redis/SeaweedFS hosts → `localhost`; Elastic `insecure_certificates`
→ `true` (the cluster cert's SAN does not cover `localhost`); `scylla.nodes` → each node's
advertised cluster IP (matching the forward, see below); `thorium.port` → `8888` (binding
`80` needs root); and `thorium.tracing.external` → `null` (the in-cluster collector is not
reachable and only produces connection noise). The raw `~/thorium.yml` is left untouched.

> Plain `minithor get-config` (no `--local`) dumps the **raw** in-cluster config, whose
> hosts are `*.svc.cluster.local` DNS names that do **not** resolve from your host. Use it
> only for tools that run inside the cluster network; use `--local` for host testing.

**3. Run the local build:**

```sh
# from the repo root (so banner.txt / UI assets resolve)
cargo build -p thorium-api --bin thorium-api          # default features include `api`
./target/debug/thorium-api --config ~/thorium.local.yml
# the API connects to redis → scylla → elastic → s3 at startup, then:
curl -sf -o /dev/null -w '%{http_code}\n' http://localhost:8888/api/health   # expect 204
```

A `204` from `/api/health` means **all** databases connected — the API establishes its
Redis, Scylla, Elastic, and S3 connections eagerly during startup and will not bind/serve
until they all succeed.

### How Scylla is exposed (and why it's special)

Redis, Elasticsearch, and SeaweedFS are single-endpoint services, so a normal
`localhost:<port>` port-forward works. **Scylla is different**: the CQL driver connects to
the seed node, then reads the cluster topology and *re-dials each node at the address it
advertises* (`broadcast_rpc_address`). With the scylla-operator each node advertises its
per-pod ClusterIP (e.g. `10.98.161.114`), which a `localhost` tunnel cannot satisfy — so a
naive forward connects, authenticates, then times out setting up the connection pool.

To make this work with only a port-forward, `minithor expose --dev`:

1. discovers each Scylla node's advertised cluster IP (its per-pod member service ClusterIP),
2. **adds that IP as a loopback alias** on your machine (`ip addr add <ip>/32 dev lo` on
   Linux, `ifconfig lo0 alias <ip>` on macOS) so the host can bind it, and
3. binds the forward to that exact IP (`kubectl port-forward --address <ip> … 9042:9042`).

The seed connection and the driver's topology re-dial then both land on the forward.
`get-config --local` writes that same advertised IP into `scylla.nodes`. The created
aliases are tracked in `~/.cache/minithor/loopback-aliases` and are removed
automatically by `expose --stop`, `cleanup --confirm`, and `minikube delete --confirm`.

> Adding a loopback alias requires `sudo` (you may be prompted). The alias is harmless and
> host-local; if a forward is killed without `expose --stop`, the alias may linger until the
> next `expose --stop`/`cleanup` or a reboot.

### Troubleshooting Local Connectivity

First confirm what is actually forwarded and which aliases exist:

```sh
minithor expose --status                 # lists running port-forwards
ip addr show dev lo | grep -w inet       # Linux: shows loopback aliases (e.g. 10.x for Scylla)
ifconfig lo0 | grep inet                 # macOS equivalent
cat ~/.cache/minithor/expose-logs/*.log  # per-forward kubectl logs
```

| Symptom | Likely cause / fix |
|---------|--------------------|
| API panics: `Failed to setup keyspace: … ConnectTimeout` or hangs after "Authenticating to Scylla" | Scylla reached but its advertised address isn't reachable. Run `minithor expose --dev` (not a plain forward) and point `scylla.nodes` at the advertised IP — easiest via `get-config --local`. Verify the loopback alias exists with `ip addr show dev lo`. |
| `expose --dev` prints `scylla: SKIPPED (no Scylla nodes found)` | The Scylla pods aren't up/ready yet, or are in a different namespace. Check `minikube kubectl -- get pods -n scylla`. |
| Elastic errors: TLS/cert hostname mismatch or `invalid peer certificate` | The cluster cert isn't valid for `localhost`. Set `elastic.insecure_certificates: true` (done automatically by `get-config --local`). |
| Elastic `403 security_exception` for `cluster:monitor/*` | Not a connection problem — the `thorium` user simply lacks that privilege. Auth succeeded; querying a Thorium index works. |
| API fails to bind / "permission denied" on port 80 | Running as non-root with `thorium.port: 80`. Use a non-privileged port (`get-config --local` sets `8888`). |
| `expose` reports a forward, but connections are refused | The forward process may have died; check its log under `~/.cache/minithor/expose-logs/` and re-run `minithor expose --dev`. Your own stale forward on a port is replaced on the next `expose`; a port held by anything else is reported and left alone. |
| Stale Scylla loopback alias after a crash | `minithor expose --stop` removes all aliases; or remove manually with `sudo ip addr del <ip>/32 dev lo` (Linux) / `sudo ifconfig lo0 -alias <ip>` (macOS). |

**macOS notes:** the loopback-alias and port-forward approach works on macOS too (it's
host-local and driver-agnostic — `kubectl port-forward` tunnels through the API server, so
no routing into the docker-driver VM is needed). minithor uses `ifconfig lo0 alias/-alias`
on macOS and falls back from `ss` to `lsof` for stale-port detection.

### Container Registry

`minithor deploy --registry` deploys a container registry in the `thorium` namespace
(`<prefix>-thorium` with `--namespace-prefix`); use `--registry-user <name>` to enable it with
basic auth (password generated; `minithor credentials --all` prints it as `registryPassword`).

| Context | Registry Address |
|---------|-----------------|
| From the host (push/pull) | `localhost:5000` (requires `minithor expose`) |
| From within Thorium (image references) | `docker-registry.thorium.svc.cluster.local:5000` (`docker-registry.<prefix>-thorium...` with a prefix) |

To push an image and reference it in Thorium:

```sh
# Tag and push from the host
docker tag myimage:latest localhost:5000/path/to/image:tag
docker push localhost:5000/path/to/image:tag
```

When configuring a Thorium image/tool to use it, reference the in-cluster address:

```
docker-registry.thorium.svc.cluster.local:5000/path/to/image:tag
```

## Step 8: Python Client

The `thorpy` Python client library (in the source repo at `python/thorpy/`) is a synchronous Python wrapper around the Rust client built with PyO3. Type stubs are in the source repo at `python/thorpy/thorium.pyi` for IDE support.

Currently implemented features are limited to: health, identify, reaction create/create_bulk, listing files and reading their results, job checkpoint and sleep, and `tool_portability(spec)` (the portability report of a JSON agentic tool spec). See the source repo at `python/thorpy/README.md` for building and usage details.

## Step 9: Key Concepts

### CaRT Format and File Downloads

All files uploaded to Thorium are stored in CaRT (encrypted/neutered) format to prevent accidental execution of malicious content. Files can be downloaded in two formats:

- **CaRT** (default): encrypted/neutered container format. Use `thorctl cart` / `thorctl uncart` to convert files locally, or download uncarted via `thorctl files download --uncarted`.
- **Encrypted ZIP**: available via the Web UI and the API at `GET /api/files/sample/:sha256/download/zip` (accepts an optional password parameter). This option is not currently available through thorctl.

### Groups

Groups are the primary access control mechanism. Files, images, pipelines, and reactions all belong to groups. Users must be members of a group to view or modify its resources.

### Images vs Pipelines vs Reactions

Images are the older form of tools: read-only now, converted to tools by the upgrade (see Tools below).

- **Images** are individual analysis tools (container-based) that run against files
- **Pipelines** are ordered chains of images that define a multi-step analysis workflow
- **Reactions** are instances of a pipeline running against one or more resources. A reaction can target:
  - **Samples** — files identified by SHA256
  - **Repos** — Git repositories (with optional branch/commit/tag)
  - **Buffers** — ephemeral data passed directly to the job
  - **None** — passing no resources creates a generic job (useful for data generation or maintenance tasks)
  
  Reactions track status, logs, and results for each stage in the pipeline.

### Tools, Workflows, and Runs

- **Tools** replace images for adding analysis tools. A Tool is published as immutable **Tool
  Instances** (`<tool>@<version>`) of kind Image (a workload), Agentic (a function a model calls),
  or Harness (the program that runs an AI agent). Pipelines name a tool for every stage.
- **AI agents**, **toolsets**, and **workflows** wire agents together; the event handler builds
  constant **workflow instances** (`<workflow>#r<revision>`) and only Ready ones run.
- A **run** is a reaction that runs one tool instance or one workflow instance directly on
  files/repos, with its own run-scoped token. A workflow run's results are stored under its result
  key (the instance name, a `~`, and the instance id).
- User docs: `{THORIUM_URL}/api/docs/user/developers/tools.html` and
  `{THORIUM_URL}/api/docs/user/users/runs.html`.

### Entities, Associations, and Trees

**Entities** are user-created objects that represent real-world things in Thorium. Entity kinds include:
- **Device** — a physical or virtual device (e.g., a router, server, IoT device)
- **Vendor** — a manufacturer or software vendor
- **Collection** — a dynamic list of items in Thorium based on search parameters like tags
- **FileSystem** — a filesystem extracted or carved from a sample
- **Folder** — a folder within a filesystem
- **Other** — anything that doesn't fit the above categories

Entities support tags, images, and group-based access control.

**Associations** are directional (or bidirectional) relationships that link entities, files, and repos to each other. Association kinds include: FileFor, DocumentationFor, FirmwareFor, AssociatedWith, DevelopedBy, ContainsCVE, ContainsCWE, BasedIn, EmployedBy, ParentCompanyOf, UsedBy, UsedIn, PerformedBy, FileSystemIn, FolderIn, FileIn, ProcessTreeIn, ChildProcess, HasNetworkConnection, SectionIn, ImportIn, FlagFor, CreatedBy, SigmaRuleHit, and InvolvedIn. Use `thorctl associations create|delete|list` to manage them; only the submitter, a group manager/owner, or an admin can delete an association.

**Trees** build relationship graphs across samples, repos, and entities to visualize how data is connected.

## Step 10: Building, Integrating & Testing Tools (Developer Workflow)

> **Images are read-only in this release.** Creating or updating an image returns `410 Gone`, so
> `thorctl images import` and `images edit` can't add tools. Add tools with `thorctl tools create
> --spec <spec>` or a toolbox, and set pipeline stages with `thorctl pipelines stage-tool` (see
> `{THORIUM_URL}/api/docs/user/developers/tools.html`).

This section is for developers **building, integrating, and testing their own tools** in Thorium:
packaging them as container images, publishing them as tool versions, wiring them into pipelines or
workflows, and distributing them as toolboxes. It builds on the container registry (Step 7) and the
tools model (Step 9).

> **Toolbox reference.** The toolbox docs are the *Toolboxes* pages of the User Docs
> (`{THORIUM_URL}/api/docs/user/developers/toolbox.html` for the format, validating, converting,
> importing, updating, exporting, diffing, removing, network policies, and offline transfer;
> `{THORIUM_URL}/api/docs/user/developers/toolbox_directories.html` for the directory layout,
> manifests, `init`, `build`, `build-images`, and image tags). If the API is offline, the same pages
> are mirrored under `https://cisagov.github.io/thorium/`.

### When to use which

- **`thorctl tools` (`create`, `publish`, `edit`, `update`, `spec`, `snapshot`, `delete`)** manage **one**
  tool and its versions from a spec file. Use them to publish a new version of a tool that exists.
- **`thorctl toolbox`** integrates **new tools** (`init tool` scaffolds a manifest and spec) and
  curates, versions, and distributes **collections** of tools, toolsets, agents, workflows, and
  pipelines as one `toolbox.json` (format 2). A legacy image toolbox is converted as it is read
  (`toolbox convert` writes the converted file).
- **`thorctl pipelines export`/`import`** copy pipelines (stage tools by name and version);
  `images export` only reads images.

### The build/test loop

```sh
# 1. Scaffold a tool (kind image|agentic|harness) or toolset|agent|workflow; -n skips prompts.
thorctl toolbox init tool --kind image -n -g <group> tools/strings
# 2. Edit tools/strings/strings.json (the spec), manifest.toml, and description.md; keep the tool under
#    a toolbox root that has a config.toml (see the directory docs).
# 3. Check it offline, then roll the manifests into toolbox.json.
thorctl toolbox build && thorctl toolbox validate toolbox.json --strict
# 4. Build and push the container images straight from toolbox.json (forks without CI).
thorctl toolbox build-images ./toolbox.json --push
# 5. See the plan, then import into an instance to test.
thorctl toolbox import ./toolbox.json --dry-run
thorctl toolbox import ./toolbox.json [--group-override <group>] [--create-groups]
# 6. Run it (Step 9), inspect results, iterate: edit, rebuild, import again.
thorctl run tool strings --sample <SHA256> --group <group> --wait
```

Importing the same toolbox twice changes nothing: versions are told apart by the hash of their spec.
`import --skip-conflicts` never changes what exists (the CI-safe choice), `--overwrite` publishes
changed specs as new versions, `--rollback-on-failure` undoes a partial apply, and `update` refreshes
only what an earlier import created.

### Distribute, diff, and apply a toolbox

```sh
# pull a collection (and with --all-kinds its toolsets, agents, and workflows) from an instance
thorctl toolbox export -g <group> -o ./tb [--all-kinds] [--with-images]
thorctl toolbox export -p <grp>/<pipe> -t <grp>/<tool> -o ./tb    # specific items; their dependencies come along
thorctl toolbox diff ./tb [--group-override <g>] [--exit-code]     # preview an import (CI drift gate)
thorctl toolbox import ./tb/toolbox.json [--image-path-prefix <reg/base>] [--update-network-policy]
thorctl toolbox remove ./tb --dry-run                              # then without --dry-run to delete what it created
```

Exporting into a directory that already has a `config.toml` reuses it (`--overwrite-config`
replaces it). **Offline / air-gapped transfer:** `export --with-images` bundles each image's tarball
(recorded in `toolbox.json`), and `import --image-path-prefix <reg>` loads, retags, and pushes them
into the offline registry. For registry addresses (`localhost:5000` vs
`docker-registry.thorium.svc.cluster.local:5000`), see Step 7.

### Single tools and pipelines

```sh
thorctl tools create --spec strings.yaml --name strings --group <group> --version 1.0.0
thorctl tools publish strings --version 1.1.0 --spec strings.yaml       # a new immutable version
thorctl tools edit strings                                              # edit the latest spec in your editor
thorctl pipelines create static --group <group> --stage strings --stage-tool strings=strings
thorctl pipelines run static --sample <SHA256> --wait --logs
```

### Typical dev loop

1. Write your tool and its Dockerfile.
2. Scaffold its Thorium config with `toolbox init tool` and fill in the spec.
3. `toolbox build` and `toolbox validate`, then `toolbox build-images --push` (or build/push manually per Step 7).
4. Apply it with `toolbox import`, or `tools create`/`tools publish` for one tool.
5. Run it with `thorctl tools run`, `pipelines run`, or `workflows run` against a sample or repo.
6. Iterate: edit the spec, rebuild, import again, or `thorctl tools edit` for a quick tweak.

## Where to Go Next

- For **full API details**: open the Swagger UI at `{THORIUM_URL}/api/docs/swagger-ui/`
- For **user workflows** (uploading, searching, reactions): read the User Docs at `{THORIUM_URL}/api/docs/user/index.html`
- For **developing tools/pipelines**: see the Developer Docs at `{THORIUM_URL}/api/docs/dev/thorium/index.html` and the Workflow Skills section
- For **packaging/distributing tools as toolboxes**: see Step 10 and the *Toolboxes* page at `{THORIUM_URL}/api/docs/user/developers/toolbox.html`
- For **admin operations** (backup, settings, provisioning): see `thoradm -h` and the docs at `{THORIUM_URL}/api/docs/user/admins/thoradm/thoradm.html`
- For **source code and internals**: browse `github.com/cisagov/thorium` and the Developer Docs at `{THORIUM_URL}/api/docs/dev/thorium/index.html`