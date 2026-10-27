# Agentic Workflow Support

This release makes every analysis job a run of a versioned **tool instance**, adds AI agents,
toolsets, and workflows that run in harnesses, adds direct tool and workflow runs with run-scoped
tokens, adds group and user model settings, and upgrades the MCP server. Read the breaking changes
and known gaps before upgrading.

### New features
---

- **Tools and tool instances** replace images for adding analysis tools. A tool is published as
  immutable versions of kind Image, Agentic, or Harness, and its latest version moves with
  publishing and deprecation. See [Tools and Tool Instances](../developers/tools.md).
- **AI agents, toolsets, and workflows**, with workflow and toolset instances built automatically by
  the event handler. See [AI Agents, Toolsets, and Workflows](../developers/agents_workflows.md).
- **Harnesses** with declared capabilities, tool binding by mechanism (Native, ThoriumApi, Cli,
  Mcp), native tools, model env exports, and orphaned run handling. See
  [Harnesses](../developers/harnesses.md).
- **Tool runs and workflow runs** on files and repos, with retries, result keys, run provenance on
  every reaction, and per-run workers holding a run-scoped token. See
  [Running Tools and Workflows](../users/runs.md).
- **Model endpoints, credentials, profiles, and user model settings**, an admin-managed allowed
  endpoint list, catalog refreshes, bans, and model resolution when a run is created. See
  [Models](../users/models.md) and [Model Endpoints for Admins](../admins/agentic/models.md).
- **MCP server**: rmcp 3.6.0 (MCP revision 2026-07-28), titles and annotations on every tool,
  structured results, the `run_tool`, `get_tool_run`, and `get_tool_run_results` tools, and a
  run-scoped tool listing. See [The Thorium MCP Server](../admins/agentic/mcp.md).
- **Thorctl** covers tools, agents, toolsets, workflows, models, pipelines, runs, and toolboxes end to
  end: create, edit, update, spec, delete, and run each kind, with interactive and non-interactive
  editing and toolboxes of tools. See [Thorctl commands](#thorctl-commands) below.
  `reactions get`, `describe`, and `logs`, `results get`, and `tokens get` and `describe` also take
  `--format` and `--watch` (and, except for `reactions logs`, `--wait-for`); the format flag is `--format`
  only on these, because their `-o` names a path. See [Spawning Reactions](../users/spawning_reactions.md).
- **Redis database**: `redis.db` (default 0) selects the Redis database the API, Thoradm, the
  search streamer, and the operator's readiness check use, so two Thorium instances can share one
  Redis server. Instances that also share a Scylla cluster still need their own
  `thorium.namespace`.
- **Inspection routes for admins**: `GET /api/events/peek/{kind}/` reads an event queue without
  moving events in flight, `GET /api/jobs/{id}` reads one job, and
  `GET /api/entities/{id}/associations` lists every association of an entity in both directions.
  They are read-only and refuse scoped tokens.

### Breaking changes
---

#### Images, pipelines, and reactions

- **Images are read-only.** `POST /api/images/` and `PATCH /api/images/{group}/{image}` return
  `410 Gone`. `thorctl images import` and `images edit` stop before they open an editor or push a
  container and print `Images can no longer be created or updated; use tools (thorctl tools
  create|edit|publish)`, and image bans are refused the same way. Image reads, lists, exports, and
  deletes keep working.
- **Every pipeline stage needs a tool** (`stage_tools`). Creating a pipeline without a tool for
  every stage is refused with `400`, and so is a reaction on a pipeline with a stage that has none.
  The upgrade pins every stage of the pipelines whose images it converts to the first tool instance
  of the stage's migrated tool; **a pipeline with a stage whose image it could not convert is left
  untouched and stops running until every one of its stages is set** with `thorctl pipelines stage-tool` (see
  [Moving a pipeline to tools](../developers/tools.md#moving-a-pipeline-to-tools)). An unknown stage
  tool id is `404`.
- Pipelines are stored in a second layout that holds the tool each stage runs (`stage_tools`). A
  pipeline left unconverted still loads, with no stage tools, until its stages are set.
- Reactions gain `source` (with run provenance), `plan`, `run`, and `run_token`. Results record
  `tool` and `tool_version`; a workflow run's results are stored under its result key, the workflow instance's name and
  id.
- Result `tool_version` values set by clients are now stored as the version's label
  (`2.3.4`) instead of JSON text that read back as a custom version.
- Entity provenance (`tools` on filesystems, decompiled functions, and process trees) holds tool
  instance ids; names stored before read back as `legacy_tools`. Create forms take `tool_ids`, update
  forms `add_tool_ids` and `remove_tool_ids`, and the old `tools`, `add_tools`, and `remove_tools`
  fields are refused with `400`.
- There are ten new entity kinds, which older clients can't deserialize. `ImageVersion` is renamed
  `ToolVersion` in the OpenAPI document (the JSON is unchanged).
- The OpenAPI documents describe every REST route with its router path (`{name}` templates, trailing
  slashes included), method, parameters, bodies, and run-token refusals. Entities, associations,
  and trees each have a document in the Swagger UI. The MCP endpoint stays out of them.

#### Tools

- A tool spec's results, children, and filesystems dependency settings name tools by id in a
  `tools` list. Image and Harness specs refuse an `images` list, and publishing one with a tool id
  that doesn't exist, isn't a tool, or isn't visible to the publisher is refused with `400`. See
  [Dependencies](../developers/configuring_images.md#dependencies).
- The default `thorium.events.max_tools_per_agent` is 128. An agent may list at most twice the limit
  in `Calls` edges and a harness may provide at most twice the limit in native tools (256 by
  default). A toolset instance still holds at most 64 members by default
  (`thorium.events.max_tools_per_toolset`).

#### Jobs, workers, and scheduling

- The stage claim route and the client's `Jobs::claim` are removed; workers claim through
  `/api/jobs/workloads/claim/...`. The `proceed` and `error` job routes need a `worker` query
  parameter.
- A workload claim's limit must be 1 to 1000 (`400` otherwise), and a claim is refused with `401`
  unless the worker was registered for the claim's group and tool instance, for admins too.
- Runtimes are served at `/api/jobs/workloads/runtimes`, and group stats list active tool instances
  under `workloads`.
- Workers, jobs, and deadlines carry `tool_instance` and `run`, and `thorctl clusters` output
  changes.
- Pods are labelled `thorium-tool-instance` and `thorium-run` instead of `pipeline`, and network
  policy `allowed_tools` matches the tool name. The agent takes `--tool-instance` instead of
  `--pipeline` and `--stage`, and no longer writes `/tmp/thorium/ai.yaml`.
- Per-run pods get the run token and model credentials as literal env values and an emptyDir at
  `/opt/thorium-keys` instead of the user's keys Secret. Keys files written by the reactor are now
  readable only by their owner.
- An agent launched with `--keys` always authenticates with its keys file; only an agent launched
  without it uses `THORIUM_URL` and `THORIUM_TOKEN`. The reactor deletes the workers of a run that
  has ended instead of launching them.
- Failing or deleting a workflow run or tool run removes its unclaimed jobs from the queues and fails
  its unfinished tool calls; failing them never completes the run.
- Resetting jobs (the scaler's reset of jobs whose worker is gone, or `POST /api/jobs/bulk/reset`)
  no longer queues again a job whose reaction already completed or failed; the job is removed from
  its queue, the running stream, and the deadline stream instead.

See [Scheduling Tools and Runs](../admins/agentic/scheduling.md).

#### Permissions and tokens

- Scoped tokens act with their own role capped at their owner's current role: a token held by a
  Developer or Analyst loses powers its role doesn't grant, and a User token can no longer create
  pipelines or override job arguments. Group create, update, and delete, user profile reads, system
  backup and restore, bulk reactions by user, and the user details list refuse scoped tokens.
- Adding a group to an entity, removing one, and writing associations check edit rights and the
  group's allow flags; adding a group that doesn't exist is `401` for non-admins.
- Bulk reaction creates check every request's samples and argument overrides.
- Updating a reaction so that a stage's options override its args needs the developer role for
  that stage's tool scaler, as creating it would (`401`).
  Replacing an ephemeral file a reaction already has needs the same role when any stage's options
  override its command, positionals, or kwargs, so an editor can't swap a script an override runs
  (`401`). Adding a new file, or replacing one on a reaction with no override, stays open to editors.
- Run-scoped tokens can't be created or updated through the token routes (`400`).
- Logging out with a run-scoped token is refused with `401` and leaves the token's value alone, so
  one worker logging out can't lock out the other workers of its run. Logging out with any other
  scoped token or a full token still rotates it.
- Deleting an entity with a scoped token needs every group the entity is in to be one of the
  token's groups, whoever submitted it (`401` otherwise, and nothing is deleted). Full tokens are
  unchanged.
- On reactions a run token only acts inside its own run (`401` for updating, proceeding, failing,
  deleting, cache edits, and stage log adds elsewhere), and its tool calls can't override args. A
  run's `run.json`, `agent.json`, and `tools.json` can't be replaced by a reaction update (`401`).
- The admin `run-token` and `run-models` routes return `404` when the token stored under a run's
  token name isn't scoped to that run, or when the run's creator can't edit in the run group (admins
  always can). The K8s scaler then fails a run that is still going with the stage log
  `run token revoked` (and the reactor deletes its workers), so a runner who is made a monitor of
  the run group or removed from it loses their running runs. Losing only the developer role doesn't
  stop them.
- A workflow run in a group its harness tool isn't in is refused with `404`.
- A run token can't create, update, or delete a pipeline, or write a definition (`401`, nothing is
  written): publish, create, deprecate, or delete a tool or a tool instance; create, change, or
  delete a toolset, an agent, a workflow, or a model credential, endpoint, or profile; ask for a
  toolset or workflow instance; delete a legacy image; ask for a model endpoint refresh; create or
  delete a graph edge (`Orchestrates`, `DelegatesTo`, `Calls`, `RunsIn`, or `Includes`); or tag or
  untag a definition. The runner's full token still can. Other tags and associations, and entities
  that hold data, are not checked.

#### Models

- User responses return the legacy `ai` endpoint `api_key` empty.
- An empty allowed endpoint list allows no model endpoints.
- A workflow run whose agents can't all resolve a model is refused with `400`.
- `thorctl models profiles create --spec` and `profiles update --parameters` refuse unknown
  parameter keys.

#### MCP server

- **Hosts not on the allow list get `403`.** The MCP server only answers requests addressed to
  loopback, the in-cluster `thorium-api` and `thorium-mcp` service names, or the host of an API url
  in the config. Add any other host clients use, such as the ingress host, to
  `thorium.mcp.allowed_hosts`; the Helm chart adds `operator.ingress.host` for you. See
  [The Host allow list](../admins/agentic/mcp.md#the-host-allow-list).
- The server is reached only over Streamable HTTP at `/api/mcp` (the SSE transport is gone).
- rmcp is now licensed Apache-2.0.

#### Thorctl

- Directories and files that `thorctl reactions logs --output`, `thorctl results get`, and
  `thorctl run` name after a pipeline, stage, or tool replace `< > : " / \ | ? *` and control
  characters with `_` and drop trailing dots and spaces. Ordinary names are unchanged.

#### Backups

- Backups taken by an earlier release restore their `Other` entities correctly; the new entity
  kinds are archived after the released ones.
- Run-scoped tokens are left out of backups, and a restore skips any that a backup holds. Other
  scoped tokens are backed up and restored as before.
- The average runtime of each tool instance (`workload_runtimes`) is backed up and restored.
- Backups hold model credential secrets in plaintext. See
  [Admin Command Line Tool](../admins/thoradm/thoradm.md).

### Fixes
---

- Result file downloads percent-encode the tool name, so the result files of a workflow run (whose
  result key holds a `#`, as in `malware-triage#r3~<WORKFLOW INSTANCE ID>`) download, including
  with `thorctl results get --tools 'malware-triage#r3~<WORKFLOW INSTANCE ID>'`.
- `thorctl reactions logs` reads a reaction's stage names from its plan, so it works for tool runs
  and workflow runs as well as pipeline reactions.
- The debug output of scoped tokens, users, Thorctl's activated scoped token, and client auth keys
  (as written into logs and error messages) shows passwords, password hashes, tokens, and
  verification tokens as `<redacted>`.

- Entity image file names must be a single file name: an empty name, `.`,
  or a name containing `/` or `..` is refused with `400` before anything is uploaded. Before, an
  absolute name could replace another entity's image.
- Listing entities by tag returns only the requested kinds, and a search made before the search
  streamer has created the search indexes answers `503` with a message saying so instead of `500`.
- The Rust client's masquerade, refresh, and revert now use the right token on every handler
  (search, entities, associations, and updates too), and a refresh no longer invalidates the
  masqueraded user's token.

### Other changes
---

- Every Scylla listing accepts 1 to 10,000 rows and stage log reads 1 to 250,000 lines, and
  refuses anything else with `400`; rows that share a timestamp are listed exactly once.
- Login refuses a stored password hash that is not argon2id with Thorium's costs.
- A bulk reaction create for several users reports a user whose requests are refused as one error
  per request for that user, instead of failing every user's requests.
- New user icons are stored under the Thorium namespace in the graphics bucket; icons at the old
  key are still served and deleted.
- Scylla setup adds the result and association columns that keyspaces from older releases lack,
  which needs `ALTER` permission the first time it runs.
- `GET /api/system/backup` answers `422` with every problem it finds instead of returning a backup
  that cannot be restored, and a restore validates the whole backup before it changes anything.
  Thoradm takes complete, verifiable backups (archive format 2, a manifest, `backup verify` and
  `backup scrub`), and exits `2` when it finished with recorded failures. See
  [Taking Backups](../admins/thoradm/taking_backups.md).
- Thorium deploys from Helm charts and the operator runs revision-based upgrades. See
  [Install with Helm](../admins/deploy/deploy-helm.md) and
  [Upgrading Thorium](../admins/deploy/upgrades.md).
### Thorctl commands
---

#### New commands

- **Tools:** `tools update`, `tools edit` (a spec in your editor, published as a new version, or the
  tool with `--root`), `tools spec`, `tools delete` (a version, or `--all-versions`), and `tools
  snapshot` (convert a stored image). `tools publish` gains `--bump`, `--from-latest`, `--set`, and
  `--dry-run`; `tools create` gains `--tag`, `--provides`, `--harness-env`, `--disable-model`, and
  `--dry-run`; `tools get` gains `--kind`, `--tag`, and `--no-limit`; `tools runs logs` prints a tool
  run's logs.
- **Agents, toolsets, workflows:** `update`, `edit`, `spec`, and `delete` for each, `agents
  unlink-tool`, `unlink-toolset`, `undelegate`, and `clear-harness`, `toolsets create --spec`,
  `workflows create --spec`, and `agents create --tool|--toolset|--sub-agent|--harness|--model-auth`.
- **Instances:** `instances request workflow|toolset` builds an instance now, and `workflows run` takes
  a workflow name for its latest Ready instance.
- **Models:** `models endpoints|profiles edit`, `spec`, and document forms of `update`.
- **Pipelines:** `pipelines create` with `--stage` and `--stage-tool`, `pipelines update`, `pipelines
  run`, and `pipelines stage-tool --remove`.
- **Runs and reactions:** `runs logs`, `runs cancel`, `reactions fail`, `reactions update` (tags),
  `runs get --kind tool|workflow|pipeline` (with `--batch` and `--status`), bulk `runs cancel` by `--batch`,
  `--workflow`, `--status`, and `--group`, `run tool|workflow|pipeline <NAME>`, and the shared run flags (`--wait`,
  `--wait-for`, `--watch`, `--output`, `--logs`, `--tag`, `--sla`, `--sample-list`, `--repo-list`, `-o/--format`, and
  `--one-per-input`, `--batch`, `--dry-run` for one run per input, with stdin lists) on `tools run`, `workflows run`,
  and `pipelines run`. See [Batch runs](../users/runs.md#batch-runs).
- **Groups:** `groups create`.
- **Toolboxes:** `toolbox validate`, `list`, `convert`, `update`, and `init tool`, with `toolbox
  import`, `export`, `diff`, `remove`, `build`, and `build-images` reading and writing tools (see
  below). See [Editing Resources](../developers/editing_resources.md) for the flags the edit commands
  share.

#### Behavior changes

- **`toolbox import` works again**, for both toolbox formats. A legacy image toolbox is converted to
  tools in memory (`toolbox convert` writes the converted file), tool versions are told apart by
  spec hash, and importing twice changes nothing. Importing the repository's toolbox needs an admin,
  because its tools run as root. The chart's toolbox import Job and `minithor deploy` import again; the
  Job gains `operator.toolbox.createGroups` and `strict`, passes `thorctl`'s exit code through, and
  keeps `backoffLimit: 6`. See [Toolboxes](../developers/toolbox.md) and [Importing a
  toolbox](../admins/deploy/deploy-helm.md#importing-a-toolbox).
- `tools run --output` implies `--wait`, and every wait ends after `--timeout` (10 minutes by
  default) with exit code `3`; it used to wait without a limit. A tool run prints one JSON object
  unless you pass `--logs`, `--watch`, `--wait-for`, or `-o` next to `--wait` or `--output`.
- `pipelines edit` now asks you to confirm the changes after an editor session, like every `edit`
  command (`--skip-confirm` or `--yes` skips it); it used to apply them directly.
- `pipelines export` and `pipelines import` carry each stage's tool (`stage_tools`) and no longer
  export or import images. An export made before this release has no stage tools and fails to import,
  before any request, naming the stages that need one.
- Image write commands (`images import`, `images edit`, image bans) print a pointer to the tool
  commands instead of the raw `410 Gone`.
- `toolbox build` writes format 2 when the directory holds tool, toolset, agent, or workflow manifests,
  and still writes the legacy format, byte for byte, for legacy manifests alone.
- `toolbox export` writes format 2 (tools with every version's spec, and with `--all-kinds` toolsets,
  agents, and workflows); `toolbox diff` and `toolbox remove` compare and delete tools by spec hash
  and dependency order.

### Known gaps
---

- **Web UI:** the Web UI has not been updated for this release. Image and pipeline pages fail, the
  reaction page can't show runs, the stats page throws, and provenance edits are rejected. There are
  no pages for tools, agents, toolsets, workflows, models, or runs. Use Thorctl or the API.
- **Reactor launchers:** the BareMetal, Windows, and Kvm launchers have not been tested on live
  nodes with tool instances and may be broken. K8s is the tested environment, and harnesses run only
  on K8s.
- Run-scoped tokens and model credentials are visible to anyone who can read pod specs where a run
  executes.

### Upgrading
---

1. Take a backup, let running reactions finish, and set the target revision `2026-10-v04` with an
   approval for each conversion step. The upgrade converts every image to a tool, sets the tool of
   each stage of the pipelines it can convert, and moves stored provenance names to
   `legacy_tools`; the API, the scalers, the event handler, and the search streamer are down while
   it does. See [Upgrading to Agentic Workflows](../admins/agentic/upgrading.md) and
   [Upgrading Thorium](../admins/deploy/upgrades.md#2026-10-v04).
2. Allow the model endpoints your groups need (see
   [The allowed endpoint list](../admins/agentic/models.md#the-allowed-endpoint-list)).
3. Make sure MCP clients reach Thorium by an allowed host (see above).
4. Have developers create a tool for each image the upgrade left unconverted and set every stage of
   each pipeline that uses one with `thorctl pipelines stage-tool` (see
   [Pipelines the upgrade leaves alone](../admins/agentic/upgrading.md#pipelines-the-upgrade-leaves-alone));
   until then those pipelines can't run.
5. Move external workers to the workload claim route, and register them as an admin.
