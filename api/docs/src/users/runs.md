# Running Tools and Workflows

Thorctl can run three kinds of things directly on files and repos. Each run is a reaction, and the
three commands share their input, waiting, and output flags:

| | Tool run | Workflow run | Pipeline run |
| --- | --- | --- | --- |
| What runs | One tool instance as one job: an Image tool, or the Image tool a container-backed agentic tool pins | A Ready workflow instance: its entry agent's harness, which calls the agents' tools and sub-agents | The pipeline's stages in order, each stage's tool |
| Command | `thorctl run tool` (or `thorctl tools run`) | `thorctl run workflow` (or `thorctl workflows run`) | `thorctl run pipeline` (or `thorctl pipelines run`) |
| Target | `<tool>@<version>`, a tool name (its latest version), or an instance id | A workflow name (its latest Ready instance), `<workflow>#r<revision>`, or an instance id | A pipeline name or `<group>/<name>` |
| Prints without a wait flag | One JSON object | The reaction id | The reaction id |
| Only this command | `--input`, `--arg`, `--kwarg`, `--switch` | | |
| `--parent` | yes | no | yes |
| Check on it later | `thorctl tools runs status\|results\|logs` | `thorctl runs describe\|logs` | `thorctl runs describe\|logs` |

`thorctl run <KIND> <NAME>` is the one place to start any of them: the kind (`tool`, `workflow`, or
`pipeline`) is always typed, never guessed from the name. Each kind runs through the same code as
the command in its own group (`tools run`, `workflows run`, `pipelines run`), which keep working
with the same flags and exit codes. Kind-only flags (a tool's `--arg`, `--kwarg`, `--switch`, and
`--input`; a pipeline's `--parent`) belong to that kind. A pipeline is a static workflow: its stages run in
order and each runs a tool, while workflows are the agentic path.

Pipelines are also started by [Spawning Reactions](./spawning_reactions.md) (`thorctl reactions
create` and `thorctl files upload -p`), and `thorctl files upload` also starts workflow and tool runs on the files it
uploads (`--workflows` and `--tools`; see [Running Workflows and Tools on
Upload](./uploading_files.md#running-workflows-and-tools-on-upload)). The older form `thorctl run <PIPELINE> <SHA256|REPO>` is still
supported as a legacy form: it works whenever its first word isn't a kind. New pipelines can't be named like a kind (`tool`,
`workflow`, or `pipeline`, among other [protected names](../developers/tools.md#creating-and-publishing)); a
pipeline created earlier with such a name must be started with `thorctl run pipeline <NAME>` or
`thorctl pipelines run`. The kind forms above are preferred. How tools and workflows are built is covered in [Tools and Tool
Instances](../developers/tools.md) and [AI Agents, Toolsets, and
Workflows](../developers/agents_workflows.md). Runs are started and followed with Thorctl, the
API, or the Web UI's Launch and Runs pages.

### Flags every run command shares
---

| Flag | Meaning |
| ---- | ------- |
| `-g, --group <GROUP>` | The group to run in (see [The run group](#the-run-group)) |
| `--sample <SHA256>` | A file already in Thorium (repeatable) |
| `--file <PATH>` | A local file to upload and run on (repeatable) |
| `--repo <URL>[:<COMMITISH>]` | A repo to run on (repeatable) |
| `--sample-list <FILE>`, `--repo-list <FILE>` | Files of inputs, one per line (a `--sample-list` line may hold several SHA256s separated by commas); blank lines and lines starting with `#` are skipped. `-` reads the list from stdin (only one list can) |
| `--tag <TAG>` | A tag to add to the run's reaction |
| `--sla <SECONDS>` | The run's SLA; a tool or workflow run's SLA can't be longer than the deployment's token lifetime (see [Run-scoped tokens](#run-scoped-tokens)) |
| `--parent <REACTION>` | Make the run a sub reaction of another (tool and pipeline runs) |
| `--wait` | Block until the run is Completed or Failed |
| `--wait-for <FIELD>=<VALUE>` | Poll until a condition holds, print once, and stop (`--timeout` bounds it, default 10 minutes) |
| `--watch`, `--interval <DURATION>` | Re-read and re-print the run until interrupted, every 2 seconds by default |
| `--output <DIR>` | Wait, then download the results of a Completed run into DIR |
| `--logs` | Wait, then print the run's status updates and stage logs |
| `-o, --format <FORMAT>` | `table`, `human`, `json`, or `yaml` |
| `--one-per-input` | Start one run per file or repo instead of one run on all of them ([Batch runs](#batch-runs)) |
| `--batch <NAME>` | Tag every run with this name so the runs can be listed, followed, and cancelled together; generated with `--one-per-input` when omitted |
| `--dry-run` | Resolve the target, group, and inputs and print what would start; nothing is created or uploaded |

Without `--one-per-input` every input of a command joins one run. `--wait` conflicts
with `--wait-for` and `--watch`; `--output` and `--logs` imply `--wait` and conflict with
`--wait-for` and `--watch`; Thorctl reports these as usage errors (exit code `2`). `--timeout` (default
10 minutes) bounds every wait, so `--wait` and `--output` stop waiting after it too.

| Exit code | Meaning |
| --------- | ------- |
| `0` | The run completed, or the command succeeded without waiting |
| `1` | The run failed (the finished run is printed first), or the API refused the call |
| `2` | The flags or input were bad |
| `3` | The run hadn't finished when `--wait`, `--wait-for`, or `--output` stopped waiting; nothing is downloaded and no logs are printed |

With a wait flag the finished run is printed in `--format` (the human form by default), followed
by its logs with `--logs`. The note `Saved N files to DIR` goes to stderr, so with `-o json` stdout
holds only the run (and, with `--logs`, a second JSON document with the logs). `--output` downloads
only for a Completed run, using the layout of the older `thorctl run`: one directory per tool, and one
per input when there are several. Tool runs and workflow runs read results under their result key
(see [Results of a run](#results-of-a-run)); a pipeline's results are read stage by stage, hidden
results included.

```bash
# run each kind and wait: the exit code says how it went
thorctl tools run strings --sample <SHA256> --wait
thorctl workflows run malware-triage --sample <SHA256> --wait --logs
thorctl pipelines run static --sample <SHA256> --wait --output ./static-out
# start now and look later
thorctl pipelines run static --sample <SHA256>
thorctl runs describe <REACTION> --wait-for status=Completed --timeout 30m
```

### The run group
---

Every run happens in one group, picked from the instance's groups. It decides who can see the run,
which group model endpoints and credentials it can use, and where its jobs run. To run in a group
you need to be an owner, manager, user, or analyst there (monitors can't run), reactions must be
allowed in it, and you need the developer role for every workload the run may spawn. If you can run
in several of the instance's groups, pass `-g/--group`; without it the run is refused with `400`
listing the candidates. If you can't run in any of them, the run is refused with `401` and a reason
for each group you can see. Admins also need to be members of the run group. A workflow run in a
group that its entry agent's harness tool isn't in is refused with `404`, and nothing is queued.

### Tool runs
---

```bash
# run a tool version on a file already in Thorium and wait for its results
thorctl tools run strings@1.1.0 --sample <SHA256> --wait
# upload a local file and run on it
thorctl tools run strings@1.1.0 --file ./sample.exe --wait --output ./strings-out
# run on a repo at a commit
thorctl tools run cargo-audit@2.0.0 --repo github.com/org/project:main
# run a container-backed agentic tool with a tool call input
thorctl tools run yara-scan@1.0.0 --input '{"depth": 2, "recursive": true}' --sample <SHA256> --wait
```

The tool is given by instance id, as `<tool>@<version>`, or as a bare tool name for its latest
version (a name that is no tool is tried as an instance name). A run needs at least one input.
`--input` takes a JSON object for a container-backed agentic tool, mapped onto the job's arguments
by the tool's input mapping (SHA256s in its samples property are added to the inputs), `--arg` adds a
positional argument, `--kwarg KEY=VALUE` (repeatable; values for one key append) adds a keyword
argument, and `--switch FLAG` (repeatable) adds a switch. Without a wait flag the command returns as
soon as the run exists; with `--wait` it waits for the run to finish, and with `--output DIR` it waits
and downloads the results into DIR.

A Thorium MCP tool given to `tools run` is called through the
[MCP server](../admins/agentic/mcp.md) with your token and returns at once with no reaction.
Harness and native tools can't be run directly.

Check on a tool run later with:

```bash
thorctl tools runs status <REACTION>
thorctl tools runs results <REACTION> --output ./out
```

`results` downloads into `/tmp/thorium/tool-results/<REACTION>` by default. Both take `-g/--group`
(all of your groups are searched by default).

#### Tool run output

Without `--logs`, `--watch`, or `--wait-for`, and without `-o` next to a wait, `tools run`, `tools runs
status`, and `tools runs results` print exactly one JSON object, so scripts and models can read them
(with those flags they print the run through the shared view, as the other run commands do):

```json
{"status":"Completed","reaction":"2f1e...","result":{"functions":412},"files":[],"error":null,"retry_after_secs":null}
```

| Field | Meaning |
| ----- | ------- |
| `status` | The run's status (`Created`, `Started`, `Completed`, or `Failed`); `null` when no run was found or started |
| `reaction` | The run's reaction id; `null` for a call of a Thorium MCP tool |
| `result` | The run's result, cut to the tool's result size limit; `null` until it completes |
| `files` | The paths of the downloaded result files |
| `error` | Why the call failed or timed out |
| `retry_after_secs` | While the run works, how long to wait before checking again (1 second, doubling up to 1 minute); `null` once it's finished |

| Exit code | Meaning |
| --------- | ------- |
| `0` | The run completed, or the command succeeded without waiting |
| `1` | The run failed, or the API refused the call |
| `2` | The input or usage was bad (for example `--input` that isn't a JSON object) |
| `3` | The run hadn't finished when the command stopped waiting |

`tools runs logs <REACTION>` prints a tool run's status updates and stage logs (`--stage` limits it
to one stage).

### Workflow runs
---

```bash
thorctl workflows run "malware-triage#r3" --sample <SHA256> --file ./dropper.bin --tag case=1234
```

The workflow is given by name for its latest Ready instance, as `<workflow>#r<revision>` for one
revision, or by instance id (a workflow id also means its latest instance); only Ready instances run,
and older Ready instances keep running after newer ones are built. A workflow with no Ready instance
fails naming `thorctl instances request workflow NAME`, which builds one now (see [AI Agents,
Toolsets, and Workflows](../developers/agents_workflows.md)). When a name is resolved the line
`Running X at instance ID` goes to stderr, so stdout still holds only the reaction id. There is no
per-run question: the agents work from their prompts and the inputs.

When a workflow run is created, Thorium picks one model for every agent: each of the agent's models
in priority order (or the workflow's default models), from the run group's endpoints and then your
own, skipping anything banned, disabled, off the deployment's allowed list, below the agent's
requirements, or that the harness can't call. If any agent can't get a model, the run is refused with
`400` before anything is created, and the message lists every model with the reason for each level.
See [Models](./models.md).

Runs that are workflow runs are tagged `WorkflowRun`, `Workflow=<workflow id>`, and
`WorkflowInstance=<instance id>`.

### Pipeline runs
---

```bash
thorctl run pipeline static --sample <SHA256> --wait --logs
thorctl run pipeline analysts/static --file ./dropper.bin --wait --output ./static-out
```

The pipeline is given by name, or as `<group>/<name>` when several of your groups have one with that
name. Each stage runs its tool (see [Building Pipelines](../developers/build_pipelines.md)); a pipeline
with a stage that has no tool is refused. A pipeline run takes the shared flags above and `--parent`,
and with no wait flag prints the reaction id. Results are read stage by stage, so `--output`
downloads one directory per stage tool. The older `thorctl run <PIPELINE> <SHA256|REPO>` runs a
pipeline on one input and streams each stage's output to the terminal; `--one-per-input` runs it on
many ([Batch runs](#batch-runs)), and `thorctl reactions create` starts pipeline reactions from a tag
search (see [Spawning Reactions](./spawning_reactions.md)).

### Batch runs
---

`--one-per-input` turns a list of inputs into one run per file or repo, on any run command:

```bash
# one workflow run per SHA256 in a list, at most 20 starting at once, then wait for all of them
cat shas.txt | \
  thorctl --workers 20 run workflow malware-triage --sample-list - --one-per-input --batch nightly --wait
# look at what would start first: nothing is created and no local file is uploaded
thorctl run tool strings --sample-list shas.txt --one-per-input --dry-run
# one run per repo, with a machine readable answer
thorctl run pipeline static --repo-list repos.txt --one-per-input -o json
```

What a batch does:

- Every run gets the `--batch` tag (a generated name when you give none; it is printed first) beside
  the tags you gave, so `thorctl runs get --batch nightly` lists the batch and `thorctl runs cancel
  --batch nightly` ends it. Repeated inputs get one run. Without `--one-per-input`, `--batch` only
  adds the tag to the one run.
- Runs start a few at a time, bounded by the global `-w/--workers` flag (10 by default). A run that
  fails to start never stops the others: the API being unreachable, `429`, and `5xx` answers are retried
  up to three times with growing waits (a lost answer can leave a duplicate run, so list the batch
  before starting it again), and any other refusal fails that input at once.
- Without a wait flag the command prints one line per started run and a summary, and exits `0` when
  every run started and `1` when any did not.
- `--wait`, `--watch`, and `--output` follow the whole batch: Thorctl reads the batch's runs by their tag
  every `--interval` (2 seconds; the wait grows up to four times that while nothing changes), prints a line each
  time a run changes status, and counts only the runs it started. `--wait` stops after `--timeout` (10
  minutes); `--watch` waits as long as it takes. `--output <DIR>` downloads each Completed run's results
  into `DIR/<input>/`. `--logs` and `--wait-for` are for a single run and are refused with
  `--one-per-input`.
- The end summary counts the inputs by status and lists the failed runs, the inputs that never started
  and why, and the runs a timeout left unfinished.
- With `-o json` stdout holds one JSON line per input, written at the end, in the order of the inputs:
  `{"input": ..., "id": ..., "group": ..., "status": ..., "error": ...}`. `status` is the run's last status
  (`Created` when nothing was followed, `Completed`, `Failed`, or `Started` after a timeout) or
  `NotStarted`. The progress lines and summary go to stderr, and `-q/--quiet` leaves out the
  progress lines but keeps the summary and errors.

| Exit code | Meaning |
| --------- | ------- |
| `0` | Every run started and, when followed, completed |
| `1` | Any run failed (a cancelled run is a failed run) or any input did not start; this wins over `3` |
| `2` | The flags or input were bad |
| `3` | No run failed, but some had not finished when the wait ended |

Not available yet for run commands: choosing inputs by tag or date search (use `thorctl reactions
create` for that, see [Spawning Reactions](./spawning_reactions.md)), `--since` (a run does not record
when it was created), `--logs` in a batch, and the argument flags of `reactions create` on the
pipeline kind.

### Following runs
---

```bash
# list workflow runs in your groups (-w/--workflow narrows it to one workflow)
thorctl runs get --workflow malware-triage
# describe a tool, workflow, or pipeline run
thorctl runs describe <REACTION>
# wait until a run completes
thorctl runs describe <REACTION> --wait-for status=Completed --timeout 30m
# rerun a failed workflow run on the same inputs, in the same group
thorctl runs retry <REACTION>
# list tool or pipeline runs instead of workflow runs
thorctl runs get --kind tool
thorctl runs get --kind pipeline --group analysts
# read a run's logs, or only one stage's
thorctl runs logs <REACTION>
thorctl runs logs <REACTION> --stage triage
# list a batch, only its failures, or only its running runs
thorctl runs get --batch nightly
thorctl runs get --batch nightly --status failed
# fail a run that hasn't finished, or see what cancelling a whole batch would do and then do it
thorctl runs cancel <REACTION>
thorctl runs cancel --batch nightly --dry-run
thorctl runs cancel --batch nightly --skip-confirm
thorctl runs cancel --workflow malware-triage --status started --yes
```

`runs get` also takes `--batch <NAME>` (the runs of a batch, of any kind) and `--status
<created|started|completed|failed>`. It lists workflow runs unless you pass `--kind tool`, `--kind workflow`, or `--kind pipeline`
(`--kind` with a `--workflow` of another kind is an error). Tool and pipeline runs have no tag to list
by, so each group's reactions are listed by status and filtered by source, keeping at most `--limit`
per group; a very busy group may hide older runs.

`runs cancel` (also `thorctl reactions fail`) fails a run that hasn't finished, or every unfinished run of a
batch or workflow (`--batch`, `--workflow`, or both; a run id can't be combined with them), which ends its
tool calls (see [Run-scoped tokens](#run-scoped-tokens)). By batch or workflow it fails every unfinished run found in your
groups (`-g` narrows it, `--status created|started` narrows it further), `--workers` at a time, asks once for the
whole set, and exits `1` if any run could not be failed. It asks for confirmation at a terminal (answering
`n` or pressing Enter prints `Cancelled; nothing was changed.` on stderr and exits 0) and
needs `--skip-confirm` (alias `--yes`) anywhere else (without a terminal it exits `1`, naming the flag), prints
what it would do with `--dry-run`, and on a run that already finished says `already <status>; nothing to cancel`
and exits 0.
`thorctl reactions update <REACTION> --add-tag TAG` and `--remove-tag TAG` (at least one is required,
and `--dry-run` previews) change the tags a reaction is listed under.

`runs describe` prints the run's kind, status, and creator; its source (for a workflow run the
workflow instance, digest, entry agent, harness, result key, and the model each agent resolved to;
for a tool run the tool instance and result key; for a pipeline reaction the pipeline); its plan
(each stage and the tool instance it runs); its **provenance** (each agent's config hash and toolset
digest, and every tool instance the run used with its version, spec hash, and image, with the
harness marked); and its samples, repos, and parent. Provenance is recorded on every reaction when it
is created, so a run can always be traced back to the exact versions it used.

`runs retry` only accepts a failed workflow run. The retry is a new run linked to the failed one as
its parent, and it resolves its models again.

### Results of a run
---

A tool run's results are stored on each input under the tool's name, with the tool instance's
version as the result's `tool_version`. A workflow run's results are stored on each input under its
**result key**: the workflow instance's name when the run started, a `~`, and the workflow
instance's id, such as `malware-triage#r3~<WORKFLOW INSTANCE ID>`, so the results of two workflow
versions, or of two workflows with the same name and revision, on the same file sit side by side.
`thorctl runs describe <REACTION>` shows a run's result key, and `thorctl runs get` lists it
without the id. The harness's
`conversation.jsonl`, `traces.jsonl`, and `status.json` are kept as result files. A failed run uploads
no results; its stage logs still hold the harness's output.

Show a run's results on each of its inputs with:

```bash
thorctl results show <REACTION>
```

Download a workflow run's results and result files with `results get`, naming its result key as the
tool (quote it, since `#` starts a comment in most shells):

```bash
thorctl results get --tools 'malware-triage#r3~<WORKFLOW INSTANCE ID>' --files <SHA256>
```

A tool run's or workflow run's stage logs are read like a pipeline reaction's, with the stage names
taken from the run's plan:

```bash
thorctl reactions logs <REACTION>
thorctl reactions logs <REACTION> --output ./logs
```

The pipeline name of a tool run is `tool:<tool> v<version>` and that of a workflow run is
`workflow:<workflow instance name>`. Directories and files that `reactions logs --output`, `results get`, and
`thorctl run` name after a pipeline, stage, or tool replace the characters Windows doesn't allow in
file names (`< > : " / \ | ? *` and control characters) with `_` and drop trailing dots and spaces,
so a workflow run's logs are written under `./logs/workflow_malware-triage#r3/<REACTION>/`. Names
with only ordinary characters are used as they are.

### Run-scoped tokens
---

Each tool run and workflow run gets its own **run-scoped token**, named `run-<reaction>`, so the run's
workers never hold your full token. It acts with the `User` role, in the run group plus the groups of
the run's inputs that you are a member of, and it can only run the run's own tool instances. It can't
create workflow runs, retries, or pipeline reactions, it can't be created or changed through the
token routes, and it expires with the run's SLA and is deleted when the run ends. So a run's SLA
can't be longer than the deployment's token lifetime (90 days unless your admins changed it): a
longer one is refused with `400` and nothing is created, and a run with no SLA gets 7 days, or the
token lifetime when that is shorter. While a run is
going, `thorctl tokens get` and `thorctl tokens describe` show its token with the run and the tool
instances it may run; the token's value is never printed by default. `thorctl tokens current --show-token`
prints the value of the scoped token you have activated. `thorctl tokens create <NAME> --groups <GROUPS>`
and `tokens update <NAME>` take `--expires`, with `--add-groups`, `--remove-groups`, or
`--clear-expires` for an update. `thorctl tokens delete <NAME>...` asks `Delete scoped token NAME?` first (the
default answer is no; answering no prints `Cancelled; nothing was changed.` and exits 0). Pass `--skip-confirm`
(alias `--yes`) to delete without asking; without a terminal and without the flag it exits `1` naming the flag.

On reactions, a run token only acts inside its own run: updating, proceeding, failing, or deleting
a reaction, editing its cache, or adding stage logs to it is refused with `401` unless the reaction
is the run or one of its tool calls. Its tool calls can't override job arguments (`401`). It can't
create, update, or delete a pipeline, or write a definition (`401`, nothing is written): publish,
create, deprecate, or delete a tool or a tool instance; create, change, or delete a toolset, an
agent, a workflow, or a model credential, endpoint, or profile; ask for a toolset or workflow
instance; delete a legacy image; ask for a model endpoint refresh; create or delete a graph edge
(`Orchestrates`, `DelegatesTo`, `Calls`, `RunsIn`, or `Includes`); or tag or untag a definition. Use
your full token for those. Other routes, such as the tags on your files and repos, the other
associations, and the entities that hold data about them, don't check the run: there the token can
do whatever the `User` role allows in its groups, like any scoped token.

A run's `run.json`, `agent.json`, and `tools.json` are written only when the run is created: a
reaction update that would replace any of them is refused with `401`, for admins too.

Failing or deleting a run ends its tool calls: their jobs that no worker has claimed yet, and the
run's own queued job, are removed from the queues, and each tool call that hasn't finished is
failed. Failing these tool calls never completes the run. The run's token is deleted afterwards.

A run stays usable only while you can edit in its run group. If you stop being an owner, manager,
user, or analyst there (for example you are made a monitor or removed from the group), Thorium
stops handing the run's token and models to its workers: the scaler fails the run with the stage
log `run token revoked` when it next starts one of the run's K8s pods, and the reactor deletes the
run's BareMetal, Windows, and Kvm workers instead of launching them (see
[Per-run workers](../admins/agentic/scheduling.md#per-run-workers)). Admins can always edit, and
leaving one of the input groups that isn't the run group, or losing only the developer role, doesn't
stop a run that already exists.

The run token is written into the run's pods in plaintext, so anyone who can read pod specs where
the run executes can use it until the run ends.

### Using the API
---

The commands above call these routes, which scripts can call directly with your token (the
[REST API Reference](../architecture/api.md#rest-api-reference) served by your instance describes the
request and response of each, all of them in its `reactions` document, `/api/reactions/openapi.json`):

| Task | Route |
| ---- | ----- |
| Run a tool instance | `POST /api/tools/instances/{id}/runs` |
| Run a workflow instance | `POST /api/workflows/instances/{id}/runs` |
| Retry a failed workflow run | `POST /api/workflows/instances/{id}/runs/{reaction}/retry` |
| Run a pipeline | `POST /api/reactions/` |
| Run a pipeline on many inputs | `POST /api/reactions/bulk/` |
| Read a run | `GET /api/reactions/{group}/{id}` |
| Read a run's logs | `GET /api/reactions/logs/{group}/{id}` (and `/{stage}`) |
| List runs by tag (a batch is a tag) | `GET /api/reactions/tag/{group}/{tag}/` |
| Cancel a run | `POST /api/reactions/handle/{group}/{id}/fail` |

### Common errors
---

| Status | Usually means |
| ------ | ------------- |
| `400` | The run has no inputs, the target can't be run directly, several run groups are possible and none was named, no model could be chosen for an agent, or the SLA is longer than the deployment's token lifetime |
| `401` | You can't run the target in any of its groups, or a run-scoped token tried something outside its own run |
| `404` | The target, an input, or the run group's harness tool doesn't exist or isn't visible to you |
| `503` | A service Thorium depends on is unavailable; try again |
