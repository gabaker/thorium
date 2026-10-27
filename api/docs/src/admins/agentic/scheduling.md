# Scheduling Tools and Runs

Every job Thorium schedules runs a **tool instance**: one immutable version of a tool, whose
workload says which environment it runs in (K8s, BareMetal, Windows, Kvm, or External) and how. This
page describes what changed for the scaler, agent, and reactor, and what admins see.

### Jobs and queues
---

- Creating a reaction resolves each pipeline stage to the tool instance it runs (the stage tool's
  latest version, or a pinned version) and records it in the reaction's plan. Reactions follow
  their plan and never re-read their pipeline.
- Jobs are queued per tool instance rather than per pipeline stage: on a shared queue per group,
  tool instance, and user, or on a run's own queue when the reaction belongs to a
  [run](../../users/runs.md).
- Workers claim jobs through `/api/jobs/workloads/claim/...` with their group and tool instance.
  The old claim by pipeline and stage is removed, so external workers built against it must move to
  the workload claim, and an admin must register them.
- A claim asks for 1 to 1000 jobs; any other limit is refused with `400`. A claim is refused with
  `401` unless the claiming worker was registered for the claim's group and tool instance, and this
  holds for admins too, so the scaler and reactor only claim for the workers they registered.
- Group stats list active tool instances under `workloads` (tool instances idle for about an hour
  are pruned), and tool instance runtimes are served at `/api/jobs/workloads/runtimes`. A tool's
  first version is seeded with a 600 second runtime estimate.
- External tool instances are never scheduled.
- When the scaler resets the jobs of workers it can no longer find, or an admin resets jobs through
  `POST /api/jobs/bulk/reset`, a job whose reaction already completed or failed is not queued
  again: it is removed from its queue, the running stream, and the deadline stream, so no worker is
  spawned for it. Jobs of reactions that are still going are reset as before.

### The scaler
---

- The scaler fetches each tool instance's workload once, the first time a deadline or the stats
  name it, and keeps it in its cache. A newly cached K8s workload whose image hasn't been inspected
  triggers one early cache reload (at most every 30 seconds) so its first job doesn't wait for the
  cache to expire.
- Fair share is ranked over tool instances, and spawn limits are counted per tool instance across
  groups.
- Pods are named after the tool and its version and carry the labels
  `thorium-tool-instance=<id>`, `thorium-run=<id>` for per-run workers, and a stage label holding the
  tool's name, so network policies can select a tool by name. The `pipeline` label is gone. The agent
  in each pod runs with `--tool-instance`.
- `thorctl clusters workers` prints each worker's tool instance and run.

#### Per-run workers

Each workflow run and direct tool run gets its own workers, which hold only the run's
[run-scoped token](../../users/runs.md#run-scoped-tokens), never the user's full token.

- **K8s:** right before spawning a per-run pod, the scaler reads the run's token (and, for a
  harness pod, the run's resolved models with their credentials) through the admin-only
  `run-token` and `run-models` routes, at most 10 reads at a time, and writes them into the pod as
  literal env values (`THORIUM_URL`, `THORIUM_TOKEN`, and for harnesses `THORIUM_MODELS` and any
  values the harness's `harness_env` exports). Per-run pods get no keys Secret volume and no
  `--keys` argument; they get an emptyDir volume (`thorium-run-keys`) at `/opt/thorium-keys` that the
  agent writes the run's keys file into. **Anyone who can read pod specs in the namespace can read
  these tokens and credentials.**
- A spawn whose reads fail is recorded as a failed spawn and tried again on the next tick. A run
  whose bound model credential is banned or deleted gets `409` from `run-models`, so its harness pod
  is not spawned until the ban clears or the run's SLA passes. A run that is still Created or Started
  when its token is gone is failed once with the stage log `run token revoked`.
- `run-token` and `run-models` also return `404` while the run's creator can't edit in the run group
  (they are no longer an owner, manager, user, or analyst there; admins always can), so such a run
  is failed the same way, with the same `run token revoked` stage log, the next time the scaler
  spawns one of its pods (the reactor deletes such a run's workers, as for an ended run). Taking
  away only the creator's developer role doesn't stop a run that already exists.
- **BareMetal, Windows, and Kvm:** the scaler registers per-run workers and the reactor reads the
  run's token (see below).
- Pipeline reactions keep using shared workers with the `thorium-{user}-keys` Secret as before.

### The agent
---

- The agent takes a required `--tool-instance` in place of `--pipeline` and `--stage`. It reads
  the tool instance once at start and exits with an error naming it if it can't.
- Results of a tool job are stored under the tool's name with the tool instance's version as
  `tool_version`. A workflow run's results are stored under its result key (see
  [Running Tools and Workflows](../../users/runs.md#results-of-a-run)).
- The agent no longer writes `/tmp/thorium/ai.yaml` for jobs.
- An agent launched without `--keys` (a per-run pod) builds its client from `THORIUM_URL` and
  `THORIUM_TOKEN` when both are set. An agent launched with `--keys` always uses its keys file, even
  when a tool's own env sets those names. A
  per-run worker writes its run's keys file (mode 0600) into `/opt/thorium-keys`, and a harness worker
  writes the models file and the credential files model SDKs read.
- A harness job launches the harness after the workload's command (see
  [Harnesses](../../developers/harnesses.md)). When the harness exits, its orphaned tool runs are
  handled by its `orphaned_runs` policy before the job finishes.
- A per-run worker exits cleanly once its run has ended (its claim or worker update gets `401`).

### The reactor
---

- The reactor's launchers fetch the tool instance each worker is registered for and refuse it unless
  its workload runs in the launcher's environment and the worker's group is one of the tool
  instance's groups. Kvm workers find their golden image and definition under the run's group and
  the tool's name.
- Per-run workers get a keys file holding only their run's token, written in the usual `keys.yml`
  format under `_runs/<run>/` in the keys directory. The reactor reads the token once per run per
  node. A run with no token (`404`) has ended, so the reactor deletes that run's workers instead of
  launching them; if the read fails for another reason, only that run's workers wait for the next
  poll. A run's keys are deleted once no worker on the node serves it.
- Keys files are now readable only by their owner, including existing user files. Because a bare
  metal agent runs as an unprivileged uid, the launcher hands it a copy of its keys owned by that uid
  under `uid-<uid>/`, rewritten at every launch.
- Bare metal tools that share a uid can read each other's keys copies and act as each other's
  users; give each tenant's images a distinct `security_context` uid (only an admin can set one).

> **Warning:** the BareMetal, Windows, and Kvm launchers have unit tests but have not been tested
> on live nodes with tool instances. The Windows launcher has not been built on a Windows target,
> and the Kvm launcher has not been run against libvirt. Expect them to be broken until they are
> verified; K8s is the tested environment. Harnesses run only on K8s.
