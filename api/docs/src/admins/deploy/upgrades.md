# Upgrading Thorium

This page explains how to upgrade a Kubernetes deployment of Thorium: moving to a new Thorium
version, and the revision upgrades the operator runs when a new version changes how Thorium is
deployed. It is the reference for every revision's upgrade steps.

## Overview

An upgrade has two parts:

1. **A new version.** Deploy the new charts (or operator image) with the same values. Helm never
   updates the CRD in a chart's `crds/` directory, so the chart's pre-upgrade hook
   (`operator.crdHook`) applies the new `ThoriumCluster` CRD with `thorium-operator crd --apply`
   before Helm changes the `ThoriumCluster`; otherwise the API server would drop any field the
   new CRD adds. Without Helm (or with the hook off), apply the CRD first yourself
   (`thorium-operator crd --apply`, or `kubectl apply` of the chart's
   `crds/thoriumcluster.yaml`); the new operator also applies it when it starts.
2. **A new revision, when there is one.** A [revision](./concepts.md#revisions) names a point in
   Thorium's deployment history. When the new operator knows a newer revision than the one the
   deployment is at, it changes nothing until you set a target revision, and then runs the steps
   that bring the deployment to it.

```bash
# 1. back up (see "Before you upgrade")
# 2. deploy the new version with the same values
helm upgrade thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION \
  -n thorium -f site.yaml
# 3. check the phase and revision
kubectl -n thorium get thoriumcluster -o wide
```

If the phase is `Ready`, the upgrade is done. If it is `UpgradeRequired`, continue with
[Setting a target](#setting-a-target).

## Before you upgrade

- Back up Redis, Scylla, and S3 with `thoradm backup new`, and keep the `thorium-credentials`
  Secret and your values files (see [Backups](./operate.md#backups)). Elasticsearch is rebuilt
  from Scylla by the search streamer, so it needs no backup of its own.
- Read the notes for every revision between the deployment's and the new one (see
  [Revisions](#revisions)), and list the steps that will run:

  ```bash
  kubectl -n thorium exec deploy/api -- /app/thoradm migrate plan --from 2026-10-v01
  ```

  `thoradm migrate list` and `thoradm migrate plan` need no cluster config, so any `thoradm`
  binary of the new version works, for example one downloaded from
  `<THORIUM_URL>/api/binaries/linux/x86-64/thoradm`.
- Check that the live schema matches the revision the deployment records. Drift that overlaps a
  step blocks that step (see [Schema drift](#schema-drift)):

  ```bash
  thoradm --cluster-conf thorium.yml schema diff
  ```

## Where a deployment starts

The revision and the full history of applied steps are recorded in Scylla, in the
`schema_migrations` table that `thoradm schema apply` creates, and mirrored in the
`thorium-upgrade-state` ConfigMap in the thorium namespace (limited to the last 200 entries).
`thoradm schema status` prints the record. The first time an operator of this release sees a
namespace, it decides the starting revision from what Scylla and the namespace hold:

| Situation | Recorded revision |
|-----------|-------------------|
| No Thorium keyspace: a fresh install | The operator's latest revision |
| A keyspace with a record | The recorded revision; the ConfigMap is rebuilt from the record |
| No record, but a `thorium-upgrade-state` ConfigMap written by an operator of this release before records existed, or by `convert-to-helm.sh` | The ConfigMap's revision, copied into the record with its history |
| A keyspace restored by `thoradm backup restore` from a whole backup without a record (one taken before records existed) into a deployment that records a newer revision | `2026-10-v02`, which the restore records with a `restored` entry; the operator plans every step after it once it reads the record (see [The Schema Record](../thoradm/thoradm.md#the-schema-record)) |
| A keyspace with data, no record, and no ConfigMap: a Helm deployment from before revisions were recorded (the first Helm release), a pre-Helm deployment kept outside Helm (the `thorium.sandia.gov/allow-inline-config` annotation), or a deployment whose ConfigMap was lost | Nothing until you set `operator.cluster.upgrade.startingRevision` (`spec.upgrade.starting_revision`); the phase is `UpgradeRequired`, and the message suggests `2026-10-v01` when the `thorium-legacy-cluster` Secret or the annotation marks a pre-Helm deployment and `2026-10-v02` otherwise |

So upgrading a deployment of the first Helm release takes one more value: set
`startingRevision: 2026-10-v02` with the new chart (the operator then shows the steps up to its
latest revision), and set the target as usual:

```bash
helm upgrade thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION \
  -n thorium -f site.yaml --set operator.cluster.upgrade.startingRevision=2026-10-v02
```

The operator records the starting revision (`thoradm schema apply --seed-revision`) only after the
live schema matches that revision's schema, and refuses otherwise, naming the differences; once a
record exists the setting is reported in `status.upgrade.stale_approvals` and can be removed.

When the ConfigMap is missing, or mirrors no record, the operator reads the record through a short
`thoradm schema status` Job and rebuilds the ConfigMap from it, and Scylla wins on any difference.
It does so on its next reconcile: a change to the `ThoriumCluster` spec, a component Deployment
becoming available or unavailable, an operator restart, or the daily reconcile of a `Ready`
cluster. To rebuild a deleted ConfigMap at once, restart the operator
(`kubectl -n thorium rollout restart deployment operator`).

Before any of this, a `ThoriumCluster` that holds `thorium.secret_key` inline in `spec.config`,
names no `config_secrets`, and lacks the annotation is treated as an unconverted pre-Helm
deployment: the operator changes nothing, sets the phase to `Error` with a message saying it "has
not been converted", and checks again every 10 minutes. See
[Converting a Pre-Helm Deployment](./convert-to-helm.md) or
[Moving a pre-Helm deployment without Helm](./deploy-thorium.md#moving-a-pre-helm-deployment-without-helm).

The recorded state is never moved backwards: an operator older than the deployment's revision, or
one that can't read a state written by a newer operator, refuses to touch it.

## When an upgrade is required

When the operator knows a newer revision than the deployment's and no target is set, the
`ThoriumCluster` goes to `UpgradeRequired` with a message like:

```
This cluster is at revision 2026-10-v01 and this operator deploys revision 2026-10-v02. Nothing
is changed until spec.upgrade.target_revision is set to 2026-10-v02 (Helm value
operator.cluster.upgrade.targetRevision); the steps that will run are listed in
status.upgrade.steps
```

While it waits, the components keep running their current version and the operator neither
provisions nor labels nodes for the deployment. It checks again every hour, and at once when the
`ThoriumCluster` changes. `status.upgrade` shows `current`, `target`, `latest`, and the planned
`steps`:

```bash
kubectl -n thorium get thoriumcluster thorium \
  -o jsonpath='{range .status.upgrade.steps[*]}{.revision} {.step} [{.kind}] {.state}: {.message}{"\n"}{end}'
```

## Setting a target

| Deployment | Set |
|------------|-----|
| Helm | `operator.cluster.upgrade.targetRevision` |
| megathor | `thorium_upgrade_target_revision` |
| Without Helm | `spec.upgrade.target_revision` |
| minithor and other development clusters | `operator.cluster.upgrade.autoTargetDev: true` (minithor sets it) targets the operator's latest revision whenever no target is set |

```bash
helm upgrade thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION \
  -n thorium -f site.yaml --set operator.cluster.upgrade.targetRevision=2026-10-v02
```

or, for a `ThoriumCluster` that isn't managed by Helm:

```bash
kubectl -n thorium patch thoriumcluster thorium --type merge \
  -p '{"spec":{"upgrade":{"target_revision":"2026-10-v02"}}}'
```

Development clusters also label their Thorium namespace `thorium.sandia.gov/dev: "true"`, which
together with `autoTargetDev` marks them as development clusters. minithor, and megathor when
`thorium_upgrade_auto_target_dev` is set, put the label on for you, and both remove it when
`autoTargetDev` is off. Elsewhere, label the namespace yourself after
`helm install --create-namespace`, as a cluster admin; the chart never renders or labels its
release namespace, and the operator can only read it:

```bash
kubectl label namespace thorium thorium.sandia.gov/dev=true --overwrite
# to stop treating the cluster as a development cluster
kubectl label namespace thorium thorium.sandia.gov/dev-
```

Put the target in your values file so later upgrades keep it, and raise it on every upgrade that
brings a new revision: a deployment that reaches a target older than the operator's latest
revision waits in `UpgradeRequired` again ("set spec.upgrade.target_revision to <latest> to
continue").

A target that can't be used sets the phase to `Error`:

| Message | Cause |
|---------|-------|
| `Revision "<id>" is not a revision id like 2026-10-v02 or 2026-10-v02-pre.<change>` | The target isn't in the `YYYY-MM-vNN` format, or a feature branch's `YYYY-MM-vNN-pre.<change>` (the CRD also rejects it) |
| `Revision <id> is not known to this Thorium build (latest is <latest>)` | The target is newer than this operator's latest revision, or not in its catalog |
| `This Thorium build's catalog has no path from revision <current> to revision <target>` | The target can't be reached from the deployment's revision, for a target older than it because the edges between them have no reverse (see [Downgrading](#downgrading)): remove or change the target |
| `Step <step> of revision <revision> can't be undone, so this downgrade is refused ...` | A step on the way down to an older target has no reverse; restore from a backup taken before it ran, or keep the target |
| `Step <step> of revision <revision> finished on this cluster, but no step on the path down to the target undoes it ...` | Downgrade one revision at a time, along the edges the cluster took, or restore from a backup |
| `This cluster is at revision <current>, which is newer than this Thorium build's latest revision <latest>; ...` | The operator is older than the deployment: deploy the newer Thorium again, revert the cluster to the revision the message names with its operator (`thoradm migrate revert --to <revision>`, or lower the target), and then deploy the older one; or, when the newer operator refuses the revert as irreversible, restore from a backup taken before the step it names |
| `This cluster's upgrade state (schema version <n>) was written by a newer Thorium operator ...` | Same as above |
| `This cluster is at pre-release revision <id>, which this Thorium build doesn't know; ...` | A development cluster ran a feature branch's pre-release revision: deploy that branch's image, set the target to the base release the message names, then deploy this build |
| `This cluster is at revision <current>, older than <minimum>, the oldest revision this Thorium release upgrades from; ...` | The deployment is too old for this release: deploy the Thorium release the message names, let it finish upgrading, then this one |

## How steps run

Once a target is set, the operator runs every step between the deployment's revision and the
target, in order, in the `Upgrading` phase. After each step it records the outcome in
`thorium-upgrade-state`, so an interrupted upgrade resumes where it stopped. When the target is
the latest revision, it then reconciles the deployment as usual and deploys the new components.

| Step kind | Runs |
|-----------|------|
| `Config`, `Infra` | Automatically once a target is set |
| `Data`, `Manual` | Only with an approval (see [Approvals](#approvals)), and only when the step has work to do; a step with nothing to do is recorded as `Skipped` without one |

A step that only checks or waits and changes no data (`scylla-role`, `elastic-identity`, and
`drain-reactions`) counts as finished only within the attempt that ran it. Clearing the target,
or setting it back to the deployment's revision, before the upgrade reaches it stops the attempt:
the operator records an `upgrade-stopped` entry in the history, and the next attempt runs those
checks again before the steps that rely on them. Steps that changed data stay finished.

Clearing the target, setting it to the deployment's revision, or setting a target the operator
can't use while a step's migration Job runs stops the Job. The operator deletes it in the
foreground, so its pod finishes its batch, writes its checkpoint, and is gone before the Job is,
and records a `Blocked` entry for the step in the history; meanwhile the message ends "Waiting for
the migration Job `<job>` to stop and write its checkpoint", and the operator checks every 15
seconds. A Job that already ended is recorded as it ended, done or failed. The components the
upgrade paused stay at zero replicas, since the older pods must not run on partly converted data,
and the cluster's message names the stopped step, its Job, the paused components with their
recorded counts, and the two ways forward. Don't scale the paused components up by hand. Either
set the target again, and the step resumes from its checkpoints under its approval (a step that
loses data needs a new confirmation code, as after removing its approval), or roll back by
restoring the backup the approval names (see [Rolling back](#rolling-back)).

A step waiting on a backend (for example Scylla not accepting connections yet) is checked again
every 15 seconds; a step blocked on an admin is checked again every minute. While a step is
blocked or waiting, the operator changes no component Deployments, so manual work such as
scaling a component down isn't undone. A failed step is reported in `status.upgrade.steps` and
retried.

The API checks the schema applied to Scylla when it starts and refuses to start, naming what to
do, when the applied schema isn't one it can run on (see [The API refuses to
start](./troubleshooting.md#the-api-refuses-to-start)). A pod of the old release that restarts
after the operator applied the new schema can refuse when the new schema changes what it reads
incompatibly; it runs again once the new release has rolled out. Leave the pods alone while an
upgrade runs.

The ConfigMap's `state.json` keeps the revision, the operator version that recorded it, and the
history of every step (up to 200 entries); its labels `thorium.sandia.gov/state-revision`,
`thorium.sandia.gov/state-version`, and `thorium.sandia.gov/state-hash` summarize it:

```bash
kubectl -n thorium get configmap thorium-upgrade-state -o jsonpath='{.data.state\.json}' | jq .
```

## Approvals

A `Data` or `Manual` step that has work to do blocks until it is approved. Its message names the
work and says it is "Waiting for an approval":

```
... Waiting for an approval: step elastic-reindex-keyword-mappings changes data and needs an
approval: add {step: elastic-reindex-keyword-mappings, backup: <where your backup is>} to
spec.upgrade.approvals (Helm value operator.cluster.upgrade.approvals). The operator does not take
backups; the backup is your reference to one you took yourself.
```

Approve it by naming the backup you took (an approval with an empty `backup` doesn't count). An
approval can also carry `acceptIssues` (see [Data migrations](#data-migrations)) and, for a step
that loses data in a downgrade, `acceptLoss`, `confirm`, and `acceptBackupGaps` (see [Accepting
loss](#accepting-loss)):

```yaml
operator:
  cluster:
    upgrade:
      targetRevision: 2026-10-v02
      approvals:
        - step: elastic-reindex-keyword-mappings
          backup: thoradm backup 2026-10-08 on /mnt/backups
```

megathor takes the same list as `thorium_upgrade_approvals`, and a `ThoriumCluster` without Helm
as `spec.upgrade.approvals` (`[{step, backup, acceptLoss, confirm, acceptIssues,
acceptBackupGaps}]`, the last four optional, in snake case (`accept_loss`, `confirm`,
`accept_issues`, `accept_backup_gaps`) in the `ThoriumCluster`).

The operator never takes backups itself; `backup` only records where yours is. A step the operator
can't run on its own reports "Waiting for manual work" with the procedure to follow once approved;
when the work is done the operator notices on its next check, records the step as `Skipped` with a
message that starts "fixed manually", and continues. No step of this release's catalog works this
way: the operator runs every `Data` step as migration Jobs (see [Data
migrations](#data-migrations)).

## Data migrations

A `Data` step that thoradm performs, such as moving rows to a new layout, runs as Kubernetes Jobs
the operator creates in the Thorium namespace. Every Job runs the deployed Thorium image as its
unprivileged user under the `thorium-jobs` service account, which holds no role and mounts no
token, with the config Secret read-only. The step goes through these stages.

1. **Check.** When the step is planned, the operator runs `thoradm migrate check --id <migration>`
   as a read-only Job that reads every object the migration would touch and changes nothing. What
   it found is shown in the step's message in `status.upgrade.steps` and in the message that asks
   for the approval: the objects by class (`current`, `migratable`, `unrecognized`, `error`), the
   rows it would rewrite, about how long the components it pauses stay down, and the five largest
   issue groups by reason code. The report is kept, so a blocked step doesn't run it again every
   minute. It runs again when the step's state changed (a step it reads after finished, the
   recorded revision, or the image), when the approvals set for the step changed, when a check
   that asked to wait comes due, and when you set the `thorium.sandia.gov/refresh-check`
   annotation on the `ThoriumCluster` to a value it didn't hold before. Checks run one at a time.
   To see every group and sample keys, run `thoradm migrate check --id <migration> --all
   --samples` yourself.
2. **Approval.** The message names the backup to take first: take it with `thoradm backup new`,
   and read the backup limitations in [Taking Backups](../thoradm/taking_backups.md) first. Then
   approve the step with `{step, backup}`. A step whose check finds nothing to change is skipped
   without an approval. A check that can't judge yet (for example while workers are still
   registered) puts the step in `Waiting` with its reason and is run again after the wait it asked
   for, between 5 and 600 seconds; an approval given meanwhile starts nothing.
3. **Pause.** The components the step lists under `pause` are scaled to zero, and their replica
   counts are recorded on the step's `Running` history entry, which the migration Job writes into
   the deployment's record as its first action, so the counts survive a lost state ConfigMap. The
   Job starts only once their pods have stopped. At most one writing thoradm Job (schema apply,
   migration, or record) runs in a namespace at a time, counting pods that haven't terminated, so
   a second one waits and says which Job it waits for.
4. **Run.** The Job runs `thoradm migrate run` and keeps a checkpoint after every batch. A process
   failure (a killed or evicted pod, a lost connection) is retried by the Job with backoff and
   resumes from the checkpoint; an eviction doesn't use up the retries. A run that fails on issue
   groups over the step's limit, or fails systematically, is not retried: the Job fails at once,
   the step is `Failed`, and its message and `status.upgrade.failure` name the Job, its pod, the
   five largest groups, and the last 20 log lines of the pod (log lines never reach the history).
   An operator that restarts while a step runs re-attaches to its Job, or starts a Job that resumes
   if it is gone. Removing the approval while a Job runs deletes the Job in the foreground, so its
   pod writes its checkpoint and is gone before the step returns to `Blocked`. Clearing the
   target, setting it to the deployment's revision, or setting one the operator can't use stops
   the Job the same way and holds the cluster (see [How steps run](#how-steps-run)).
5. **Restore.** The paused components stay at zero while the later steps run, and while the
   cluster is held after a stopped step. Once every step is
   done, the operator applies the new release's Deployments with them still at zero, and scales
   each to its recorded count. A count that was lost before the Job's first write is replaced by
   the replica count the `ThoriumCluster` declares, and the step's message says so.

A `Failed` step is retried by changing its approval: a different `backup` (normally a newer
backup), or an `acceptIssues` list. A step whose own Redis layout shows as drift (see [Schema
drift](#schema-drift)) is retried the same way. `acceptIssues` holds reason codes whose issue groups don't fail
the run, taken from the check report or the failed run's status:

```yaml
approvals:
  - step: users-settings-theme
    backup: thoradm backup 2026-10-23 on /mnt/backups
    acceptIssues: ["malformed_json:settings"]
```

An accepted code is used up by the run it was given for. A code that matches no group is ignored
and named in the status. A run that finishes with groups within the step's limit, or accepted,
finishes the step and sets the `MigrationIssues` condition in `status.upgrade.conditions`. The
condition lists every step of the running upgrade that ended with issues, separated by ` | `, this
run's step first and the others newest first, each as `<step>: <n> objects in <m> groups; thoradm migrate issues
--id <migration>`; this run's step also names its largest groups' reason codes. It lists at most
five steps and ends with "and <n> more steps with issues" when there are more, and it clears once
no step of the upgrade has issues left, so a later step that ended clean doesn't hide an earlier
one's. To fix a group by hand, correct its objects and run `thoradm migrate run --id <migration>
--reason <code>`; when that ends the pass, the operator continues without a new approval.

The operator plans no data-changing step while the image's thoradm knows another catalog than the
operator: the `CatalogSkew` condition names both, and the schema-apply Job still runs. Deploy the
operator and the image of the same release to clear it. Approvals that name no step of the plan
(already done, another revision, or misspelled) are listed in `status.upgrade.stale_approvals`.

### Changing a partition size

A partition size changed while rows of its data type exist moves where those rows belong, so the
schema-apply Job refuses it before any component rolls out. The operator then plans the step
`rebucket-<data type>` (`rebucket-files`, `rebucket-repos`, `rebucket-results`,
`rebucket-entities`, `rebucket-associations`, or `rebucket-tags`): a `Data` step that is checked
and approved as above, pauses the API and every component that writes Scylla, and moves the rows
in place. Its message shows the rows to move and about how long it takes. Once it finished the
schema apply runs again. A refused `files.password`, or a refused change of another setting, plans
no step; the status names the setting. After the step, a major compaction of the moved tables
removes the tombstones the move left.

### Rebuilding Elasticsearch indexes

Elasticsearch holds only data the search streamer can rebuild from Scylla, so a change to an
index's mappings is never migrated in place. A step that names the `elastic-rebuild` migration, such
as `elastic-reindex-keyword-mappings` of `2026-10-v02`, is checked and approved like any other data
step, and then differs in what it pauses and how it ends.

1. **Pause.** Only the search streamer is scaled to zero, and its pods have stopped before the Job
   starts.
2. **Run.** The Job deletes every Elasticsearch index that is missing or whose mappings differ from
   the ones this release declares, and then the search streamer's init session keys of the sources
   (results and tags) those indexes hold. Scylla is not touched. The streamer's event queue and
   in-flight keys are left alone, so events written meanwhile are replayed afterwards as upserts.
3. **Rollout.** The search streamer stays at zero until the operator has applied the new release's
   Deployments, so nothing writes to a missing index and Elasticsearch never creates it with
   dynamic mappings.
4. **Reingest.** The step is now `Reingesting`: the Job is finished, and the search streamer,
   running the new image, recreates the indexes and streams every result and tag from Scylla into
   them. A `Reingesting` step counts as finished, so the upgrade continues and the phase goes to
   `Ready`, but search returns incomplete results until the reingest is done. The
   `SearchIncomplete` condition in `status.upgrade.conditions` is true meanwhile and names the
   sources still reingesting. After the rollout the plan is empty, so look at the condition rather
   than at `status.upgrade.steps`.

Each reconcile after the rollout reads Elasticsearch and Redis. When no init session is left and
every index has its declared mappings, the step is recorded as `Done` ("The search-streamer
finished reingesting ...") and the condition clears with "Search results are complete".

If the init sessions are gone but an index is missing or mapped differently (the session was lost,
or something wrote to an index before the streamer created it), and it still looks that way five
minutes later, the operator runs the rebuild again under the step's original approval and pauses
only the search streamer; the five minutes give a streamer that has just started time to create
its indexes. The re-run needs no new approval. A re-run whose Job fails leaves the search streamer
at zero until you change the step's approval, and the condition says why.

To rebuild by hand, with the search streamer scaled to zero, run
`thoradm migrate run --id elastic-rebuild` (see [Migrate](../thoradm/thoradm.md#migrate)); it needs
the thoradm config and takes the writer lease, like every writing command.

## Inspecting revisions

`thoradm migrate list` and `thoradm migrate plan` read the revision catalog built into thoradm and
need no cluster config:

```bash
thoradm migrate list                          # every revision and its steps
thoradm migrate plan --from 2026-10-v01       # the steps from a revision to the latest
thoradm migrate plan --from 2026-10-v01 --to 2026-10-v02
```

The other `thoradm migrate` commands (`check`, `run`, `verify`, `issues`, `report`, `record`,
`revert`, and `reset-state`) read the cluster, so they need the thoradm config and reach Scylla,
Redis, and S3. [Migrate](../thoradm/thoradm.md#migrate) describes each one, its flags, and its exit
codes. Every migration this release registers can be checked, run, and verified by thoradm,
including `elastic-rebuild`, the migration behind the Elasticsearch reindex step. A migration
thoradm can't perform prints its manual procedure and exits with code 3 on `check`, `run`, and
`verify`, and one thoradm can only check does so on `run` and `verify`; no migration of this
release is either.

## Schema drift

Drift is the live schema differing from the schema of the revision the deployment records: a
table or column dropped by hand, an Elasticsearch index created with other mappings, an object a
newer or feature-branch image applied before the deployment went back. The operator reads it
through a `thoradm schema status` Job (probing Scylla, Redis, S3, and Elasticsearch, read-only)
when it plans an upgrade, after every schema apply, whenever the acknowledgements change, and at
most once an hour otherwise. A read that fails leaves the status as it was and is tried again after
five minutes; while an upgrade is being planned it holds the plan until a read succeeds. Drift
never holds a normal reconcile.

Each object is `missing` (expected and absent), `changed` (present but different: a column type, a
key, a view's columns, a mapping), or `unexpected` (present and unknown to the revision).
`status.upgrade.schema` shows each backend as `ok` or `drift`, and `status.upgrade.drift` holds the
counts of each kind, the number of acknowledged objects (`acknowledged`, of which `unfingerprinted`
never lapse), and the first 20 unacknowledged objects with their observed fingerprints (`more`
counts the rest). The `SchemaDrift` condition summarizes it, for example `scylla, elastic: 1
missing, 1 changed, 1 unexpected not acknowledged (thoradm schema diff for details)`.
`thoradm schema diff` prints every object.

How each kind resolves:

| Drift | What happens |
|-------|--------------|
| `missing` | Reported. The next schema apply creates a missing table, column, or view; it never blocks a step |
| `changed` in Elasticsearch | Reported. A rebuild of the index resolves it (see [Rebuilding Elasticsearch indexes](#rebuilding-elasticsearch-indexes)); it never blocks a step |
| `changed` in Scylla, Redis, or S3 | Blocks every step whose targets overlap the object, until you restore it. The exception is a changed layout of a Redis object, which doesn't block the step that converts that layout (the schema apply records the new layout before the conversion runs) |
| `unexpected` | Never dropped by the operator or by a schema apply. Blocks every step whose targets overlap it until you acknowledge it |

A blocked step stays `Blocked` with a message such as `Step <step> targets schema objects that
differ from the recorded revision: scylla samples_list.preview (unexpected). Restore them to the
recorded definition, or acknowledge an unexpected object in spec.upgrade.drift_acknowledged;
thoradm schema diff has the details`. Steps whose targets overlap no drifted object run as usual.
The status lists only 20 objects, but the operator also keeps up to 200 more objects that would
block a step, so a step blocks however many objects drift up to that. Beyond them the
operator can't tell which ones it left out, so it blocks every step that has targets, and the
message ends with "and more than the 200 the status carries"; restore or acknowledge objects until
the blocking ones fit, using `thoradm schema diff` for the full list.

Acknowledge an `unexpected` object by copying its backend and name from `thoradm schema diff` or
from `status.upgrade.drift.objects`, with its fingerprint when you want the acknowledgement to
lapse if the object changes again:

```yaml
operator:
  cluster:
    upgrade:
      driftAcknowledged:
        - backend: scylla
          object: samples_list.preview
          fingerprint: 81aa5c0e...
```

The key for a `ThoriumCluster` without Helm is `spec.upgrade.drift_acknowledged`, and megathor takes
the list as `thorium_upgrade_drift_acknowledged`. The `backend` is `scylla`, `elastic`, `redis`, or
`s3`. An acknowledgement with a fingerprint covers the object only while it still has that
fingerprint, and the object is reported again, with a note that its acknowledgement lapsed, once it
changes. One without a fingerprint covers whatever the object becomes, so the status counts those
separately as `unfingerprinted`. The fingerprint of a single column is a hash of the column and its
type, so an acknowledgement of `samples_list.preview` lapses when that column's type changes but not
when another column of the table does. Only `unexpected` objects can be acknowledged. The `events`
table and `entities_by_name` view that older releases created are acknowledged for you, and
`thoradm schema drop-legacy` can remove them (see
[Legacy Schema Objects](../thoradm/thoradm.md#legacy-schema-objects)).

While a schema apply is the first step of an upgrade, the live schema holds the objects of the
revision being applied before the record moves to it. Drift is compared with the revision the
applied schema belongs to, so those objects aren't reported as unexpected in the middle of an
upgrade.

## Downgrading

A target older than the deployment's revision is a downgrade. The planner follows the reverse
edges of the revision graph back to the target, and the newer release runs the downgrade, because
only it knows how to undo its own changes. A downgrade is refused before anything runs when:

- the catalog has no path down to the target (the revision list in
  `thoradm migrate list` marks an edge `one-way` when it has no reverse edge), with the message
  `This Thorium build's catalog has no path from revision <from> to revision <to>`;
- a step on the path can't be undone: `Step <step> of revision <revision> can't be undone, so this
  downgrade is refused before anything runs; restore from backup instead (a backup taken before
  step <step> ran)`;
- a step finished on this cluster and no step on the path undoes it: `downgrade along the edges the
  cluster took, targeting each revision it passed through in turn, or restore from backup`;
- an upgrade stopped partway along an edge above the recorded revision after one of the edge's
  steps ran (it is recorded done or skipped, or it started and was stopped or failed), and that
  step can't be undone or changed data that no step on the path undoes: the messages above name
  it. The path down from the recorded revision never takes that edge's reverse, so the step's data
  would stay as the newer release left it.

The order of operations:

1. With the newer operator still deployed, lower `targetRevision` (or run
   `thoradm migrate revert --to <older revision>`; `--dry-run` prints the steps, what each loses,
   and the confirmation code of each lossy one, and changes nothing). The operator pauses the
   components the steps list, runs the reverse steps newest first, and records the older revision.
   A downgrade never reuses the approvals of the upgrade: each step that changes data needs its
   own approval naming a backup.
2. The paused components stay at zero replicas. The only Deployments the newer operator can apply
   are the newer release's, and those must not run on data reverted to the older layout. The
   recorded replica counts stay on the steps' `Running` entries in the history and are shown in
   `status.upgrade.steps`.
3. Deploy the older operator and images (`helm upgrade` to the older chart version). The older
   operator's rollout restores the paused components to their recorded counts. **An operator from
   before this feature doesn't read those counts**, so after deploying it, scale each paused
   component back by hand to the count the status showed, for example:

   ```bash
   kubectl -n thorium scale deployment api --replicas=2
   ```

   Going back to the first Helm release (revision `2026-10-v02`) takes two more steps:

   - That release's chart renders the Scylla agent's placeholder `scylla-auth-token` Secret, which
     this release leaves to the Scylla operator, so a plain `helm upgrade` refuses with `Secret
     "scylla-auth-token" in namespace "scylla" exists and cannot be imported into the current
     release`. Pass `--take-ownership` (Helm 3.17 or newer) to that `helm upgrade`.
   - Its operator patches the component Deployments without removing the pod security context
     this release sets (the unprivileged user 10001), and its scaler, which runs as root, then
     fails with `Scaler failed to initalize: IO(Os { code: 13, kind: PermissionDenied ... })`.
     Delete the component Deployments once its operator runs, and restart it so it recreates them:

     ```bash
     kubectl -n thorium delete deployment api scaler event-handler search-streamer
     kubectl -n thorium rollout restart deployment operator
     ```

     If that operator had already reported `Ready`, the phase can stay `Provisioning` after it
     recreates them, although every component runs: it compares its final `Ready` with the status
     it read when that reconcile began, which was already `Ready`, and skips the write. Restart
     the operator once more and it reports `Ready`.

An upgrade that stopped half way can be reverted the same way; only the steps that finished are
reversed, and a step reversed this way runs again when you upgrade again. A step of the edge the
upgrade stopped on, above the recorded revision, isn't reversed: when it ran and can't be undone,
or changed data no step on the path undoes, the downgrade is refused as above, and restoring the
backup is the way back. An object a reverse step doesn't recognize, or that an issue group left
unmigrated, is skipped.

### Accepting loss

A reverse step can lose data: a field that exists only in the newer layout is filled with a
default, or dropped. The step's check estimates what it loses before you approve anything (the
fields, how many objects hold a value, and whether `thoradm backup` covers each field), and the
blocked step shows it in `status.upgrade.steps` and in its message. Outside a development cluster
every lossy step is its own decision:

1. The blocked step shows a confirmation code: eight characters (Crockford base32) that depend on
   the step, the recorded revision, the fields it loses, and the length of the applied history, but
   not on the object counts, so live writes don't change the code between showing and approving it.
2. Take a backup (`thoradm backup new`) and approve the step with the code:

   ```yaml
   approvals:
     - step: <step>
       backup: thoradm backup 2026-10-23 on /mnt/backups
       acceptLoss: true
       confirm: 7Q2M0X4D
   ```

   Add `acceptBackupGaps: true` when a field the step loses isn't covered by `thoradm backup`; the
   step stays blocked and names the field until you do.
3. The code is used up when the step starts. It is recorded in the history, so it never matches
   again, not after a round trip and not after the state ConfigMap's 200-entry history drops the
   entry. A step that fails after it started needs a new approval repeating the new code the status
   shows; changing `backup` alone doesn't re-approve it. An approval given ahead for a later lossy
   step doesn't match, because a code belongs to one block of one step.

An approval whose code is missing, wrong, stale, or already used, or that lacks `acceptBackupGaps`
for an uncovered field, runs nothing; the step's message says which and shows the current code, for
example `The approval for <step> was refused: that code isn't the current one: it is wrong, or was
shown for an earlier block of the step. Repeat the current code <code> as confirm`. A downgrade over
several lossy steps stops at each one for its own code, and `thoradm migrate revert` runs one lossy
step per code.

On a development cluster, `operator.cluster.upgrade.dangerousDowngradeLossy: true` (CRD
`spec.upgrade.dangerous_downgrade_lossy`, default `false`) accepts the loss of every lossy step
without a code or a backup. The setting is honored only when the cluster has `autoTargetDev` and its
namespace carries the label `thorium.sandia.gov/dev: "true"` (see [Setting a
target](#setting-a-target)); minithor sets both. On any other cluster the setting is ignored, the
`LossySettingRefused` condition names it and how to clear it, every lossy step still needs its own
approval with its code, and the rest of the plan runs as usual.

### Handing over to the older operator

An older operator reapplies its own `ThoriumCluster` CRD, and the API server then drops fields the
older CRD doesn't know. The last step of a downgrade therefore removes every field and value the
older release doesn't know from the `ThoriumCluster` spec and status and from the upgrade state
(including the step states `Reingesting` and `Waiting`), before the older operator starts. Until
`helm upgrade` replaces it, the newer operator stays in hand-over mode: it writes only status the
older release can read (no planned newer steps, no newer conditions), it doesn't add the removed
spec fields back, and it leaves the paused components at zero. The status message names the
previous operator. The phase is `UpgradeRequired`, except for a cluster reverted to `2026-10-v02`
or earlier: the operators of those releases know only the phases `Provisioning`, `Ready`, and
`Error`, so the phase is `Error`. Raising the target again ends hand-over mode.

An operator that finds a revision it doesn't know refuses and tells you which revision to revert to
with which image (see the messages in [Setting a target](#setting-a-target)).

## Conditions

`status.upgrade.conditions` lists these conditions while they hold:

| Condition | Meaning |
|-----------|---------|
| `SchemaDrift` | The live schema differs from the recorded revision (see [Schema drift](#schema-drift)) |
| `MigrationIssues` | A migration finished with issue groups within its limit, or accepted: the message lists up to five steps of the running upgrade that ended with issues (see [Data migrations](#data-migrations)) |
| `SearchIncomplete` | An Elasticsearch rebuild's reingest hasn't finished, so search results are incomplete (see [Rebuilding Elasticsearch indexes](#rebuilding-elasticsearch-indexes)) |
| `LossySettingRefused` | `dangerousDowngradeLossy` is set on a cluster that isn't a development cluster, so it is ignored |
| `CatalogSkew` | The image's thoradm knows a different catalog than the operator (the message names both), so no data-changing step is planned until the operator and the image are of the same release |
| `SchemaSkew` | The schema-apply Job reported a deployment fingerprint different from the one the operator's build declares, so the operator and the image were built from different schemas; the rollout continues, and deploying an operator and image of the same release clears it |
| `MutableImageTag` | The cluster runs a mutable image tag such as `latest`, so a new image under the same tag changes nothing that triggers the schema apply; use a version tag, or set the `thorium.sandia.gov/retry-schema-apply` annotation on the `ThoriumCluster` to a new value after pulling a new image |

## Limitations

- The operator never takes backups, and it runs only the `Data` work thoradm performs as a
  migration Job (see [Data migrations](#data-migrations)), the Elasticsearch reindex of
  `2026-10-v02` included (see [Reindexing Elasticsearch](#reindexing-elasticsearch)).
- After `thoradm migrate reset-state` or a `thoradm backup restore` moves the recorded revision,
  the operator keeps the revision its `thorium-upgrade-state` ConfigMap holds until it next reads
  the record, when its hourly drift read is due at a reconcile (at least daily on a `Ready`
  cluster), and then rebuilds the ConfigMap from the record (see [Resetting the Recorded
  State](../thoradm/thoradm.md#resetting-the-recorded-state)).
- Revisions released as `one-way` (see `thoradm migrate list`) can't be downgraded: restore from a
  backup instead (see [Rolling back](#rolling-back)).
- Rolling the operator image back also rolls the cluster-wide `ThoriumCluster` CRD back; downgrade
  the revision first (see [Downgrading](#downgrading)) when the older release doesn't know it.

## Rolling back

- **Before you set a target**, nothing has changed: deploy the previous chart version (or
  operator image) again.
- **After steps have run**, downgrade along the reverse edges when the revisions have them (see
  [Downgrading](#downgrading)). Otherwise restore from your backups onto the previous version.
  `thoradm backup restore` converges the schema to the revision the backup was taken at, and
  `thoradm migrate reset-state` corrects the recorded revision after data was restored or wiped
  some other way (see [Migrate](../thoradm/thoradm.md#migrate)).
- **After you stopped a step** (see [How steps run](#how-steps-run)), the paused components stay
  at zero until you either set the target again, which resumes the upgrade, or restore the backup
  the step's approval names. The restore loads Redis through the API (see [Running a
  Restore](../thoradm/thoradm.md#running-a-restore)), and a step that pauses the API (such as
  `users-settings-theme` of `2026-10-v03`) leaves it at zero: once the operator is scaled to 0,
  scale the API to 1 for the restore (`kubectl -n thorium scale deployment api --replicas=1`).

## Upgrade paths

Each Thorium release upgrades a deployment from any revision between its oldest supported
revision and its own. A release keeps at least the previous four revisions and every revision of
the twelve months before it; older revisions are dropped from its catalog. A deployment at an older
revision installs the intermediate releases the table names first, in order: deploy each one, set
its latest revision as the target (see [Setting a target](#setting-a-target)), and wait for the
phase `Ready` before deploying the next. The operator refuses to upgrade from a revision older than
its oldest supported one, and its message names the release to install first.

Find the deployment's revision in the `REVISION` column of
`kubectl -n thorium get thoriumcluster -o wide`, or in the `thorium-upgrade-state` ConfigMap:

```bash
kubectl -n thorium get configmap thorium-upgrade-state \
  -o jsonpath='{.metadata.labels.thorium\.sandia\.gov/state-revision}'
```

The table lists each Thorium version that brought a new revision; a version without one deploys
the revision of the newest version listed before it. A pre-Helm deployment of any version records
its revision when it is converted (see [Converting a Pre-Helm Deployment](./convert-to-helm.md)).

<!-- upgrade-paths:begin (generated from the catalog's support floors) -->
| Thorium version | Revision | Oldest revision it upgrades from | From an older revision |
|-----------------|----------|----------------------------------|------------------------|
| 1.8.1 | `2026-10-v04` | `2026-10-v01` | — |
| Pre-Helm, any version | `2026-10-v01` once converted | — | Convert with the `convert-to-helm.sh` of Thorium 1.8.1, then upgrade from `2026-10-v01` |
<!-- upgrade-paths:end -->

## Revisions

### 2026-10-v01

The baseline: a deployment made by the minithor or megathor scripts before the Helm charts. Every
converted deployment starts here. It has no steps.

### 2026-10-v02

A deployment made by the Helm charts with the chart-managed operator. Deployments converted from
`2026-10-v01` run these steps:

| Step | Kind | What it does |
|------|------|--------------|
| `scylla-role` | Infra | Checks that Thorium's Scylla role logs in with the configured credentials, creating it first when the Scylla bootstrap is set |
| `elastic-identity` | Infra | Checks that Thorium's Elasticsearch user authenticates and holds the index privileges Thorium needs, creating it first when the Elasticsearch bootstrap is set (see [`elastic-identity` keeps failing](./troubleshooting.md#the-elastic-identity-step-keeps-failing)) |
| `elastic-reindex-keyword-mappings` | Data | Runs the `elastic-rebuild` migration as Jobs: skipped when every index exists with the mappings this release declares; otherwise needs an approval, pauses the search streamer, drops the indexes that are missing or mapped differently, and is `Reingesting` until the search streamer has recreated and refilled them (see [Reindexing Elasticsearch](#reindexing-elasticsearch)) |
| `finalize` | Config | Records the new revision |

megathor deployments made before the indexes were left to the search streamer created
`thorium_repo_results` and `thorium_repo_tags` without mappings, so their `group` field is `text`
and group filters can miss documents; those deployments need the reindex. They also created
`thorium_file_results`, `thorium_file_tags`, and `thorium` indexes that Thorium never uses, which
are safe to delete.

### 2026-10-v03

Adds the step that applies the schema before any step that changes data, and the step that moves
each user's theme into the user's settings. Deployments at `2026-10-v02` run these steps when a
target is set:

| Step | Kind | What it does |
|------|------|--------------|
| `schema-apply` | Infra | Runs `thoradm schema apply` in a Job, creating the tables, columns, and views this release declares and recording what was applied, before any later step reads or writes them |
| `users-settings-theme` | Data | Moves each user's theme from its own field of the user's hash into the user's `settings`, as a thoradm migration Job that pauses the API. It needs an approval only when some user still holds the theme in its own field (users written by a development image); on every other deployment it finds every user already converted and is recorded `Skipped` without one. A user whose `settings` or theme can't be read is an issue group whose reason code names the field, such as `malformed_json:settings` or `unknown_variant:theme`, and the step's limit is 0 groups, because one user the API can't load breaks every user listing: fix the user, or accept its code in the approval's `acceptIssues` (see [Data migrations](#data-migrations)) |

The operator runs the same Job before it rolls out any component whenever its inputs change (the
image, the revision, the data-shaping settings, the table TTLs, or the files password). A Job that
fails sets the phase to `Error`, rolls nothing out, and isn't retried by itself: its logs are in
`status.upgrade.failure`, and setting the `thorium.sandia.gov/retry-schema-apply` annotation on the
`ThoriumCluster` to a new value (or changing what the Job's inputs cover) runs it again.

Downgrading to `2026-10-v02` runs `users-settings-theme-reverse`, a Data step that pauses the API.
`2026-10-v02` reads the theme inside each user's `settings`, the layout this revision keeps, so the
step leaves every user in that layout: it skips each user already there, with its theme, and moves
the theme of a user still holding it in its own field into the user's `settings`. A downgrade across
this revision therefore keeps every user readable by `2026-10-v02`. It loses no data, and on a
deployment that never held such a user it finds nothing to convert and is recorded `Skipped`
without an approval. A user it can't read is an issue group with the step's limit of 0 groups, like
the step it reverses. The schema this revision applies is not undone: `2026-10-v02` ignores the
columns it doesn't read.

<!-- PLACEHOLDER: the section of the next released revision goes here, after 2026-10-v03 and
before this heading; add its step table and, if a step waits on running API pods, its warning to
"How steps run". -->

## Reindexing Elasticsearch

Elasticsearch holds only data the search streamer can rebuild from Scylla. The
`elastic-reindex-keyword-mappings` step of `2026-10-v02` rebuilds the indexes whose mappings are
wrong, such as a `group` field that isn't a keyword, through the `elastic-rebuild` migration:

1. Its check Job (`thoradm migrate check --id elastic-rebuild`) counts the Thorium indexes that are
   missing or mapped differently from what this release declares. With none, the step is recorded
   `Skipped` without an approval.
2. Otherwise the step waits for an approval. Approve it (see [Approvals](#approvals)) with a
   reference to your Scylla backup, since the search streamer refills the indexes from Scylla.
   `thoradm migrate check --id elastic-rebuild --samples` names the indexes it would drop.
3. The operator scales the search streamer to zero and runs the migration Job, which drops those
   indexes as Thorium's Elasticsearch user (so that user needs `delete_index` on them; the chart's
   role grants `all` on `thorium*`) and the search streamer's init session keys. The step is then
   `Reingesting`, the upgrade continues, and after the rollout the search streamer, on the new
   image, recreates the indexes and streams every result and tag from Scylla into them (see
   [Rebuilding Elasticsearch indexes](#rebuilding-elasticsearch-indexes)).

```bash
# the step's state and message, and the search streamer's progress after the rollout
kubectl -n thorium get thoriumcluster thorium -o json \
  | jq '.status.upgrade | (.steps, (.conditions[]? | select(.type == "SearchIncomplete")))'
```

Searches return incomplete results until the search streamer has streamed everything back; the
`SearchIncomplete` condition clears when it has, and the step is recorded `Done`.

On a `Ready` deployment (outside an upgrade), the search streamer can rebuild its indexes itself
with `--reindex`, which deletes and recreates them and needs `delete_index` on them. Component
`args` replace the defaults, so set the full list, wait for the reindex to finish, and then remove
it again (otherwise every restart reindexes):

```yaml
operator:
  cluster:
    components:
      search_streamer:
        args: ["--config", "/conf/thorium.yml", "--keys", "/keys/keys.yml", "--reindex"]
```

## Tool notes

- **minithor** sets `autoTargetDev` and `dangerousDowngradeLossy`, and labels the namespace, so
  `minithor deploy` upgrades the deployment to the latest revision and a downgrade accepts its
  loss without codes. It stops with an error when a step is blocked; pass the approval with
  `--set 'operator.cluster.upgrade.approvals[0].step=<step>' --set 'operator.cluster.upgrade.approvals[0].backup=<where>'`
  or a `--values` file and deploy again. A registry deployed with `--registry` by the first Helm
  release ran as root, and this release runs it as the unprivileged user 1000, which can't write
  into the directories the old registry created on storage that ignores `fsGroup` (such as
  minikube's hostpath storage): pushes then fail with `unknown error`, and the registry logs
  `mkdir /var/lib/registry/...: permission denied`. Give the data to user 1000 once with a Job
  that runs as root and mounts the `registry-persistent-storage-claim` claim
  (`chown -R 1000:1000 /var/lib/registry`).
- **megathor** accepts `UpgradeRequired` as a finished run and prints the planned steps; set
  `thorium_upgrade_target_revision` (and `thorium_upgrade_approvals`) and run the playbook again.
  It fails at once when a step is blocked or the phase is `Error`. It sets
  `dangerousDowngradeLossy` and `driftAcknowledged` from `thorium_upgrade_dangerous_downgrade_lossy`
  and `thorium_upgrade_drift_acknowledged`.
- **Without Helm**: change the operator image and `spec.version` together, then set
  `spec.upgrade.target_revision` (see
  [Deploy the Operator Without Helm](./deploy-thorium.md#upgrading)).
