# Upgrading to Agentic Workflows

This page covers the upgrade of an existing Thorium deployment to the release that adds tools, tool
instances, agents, workflows, runs, and models (see the
[release notes](../../release_notes/agentic_workflows.md)). That release stops running images
directly, so the upgrade converts the images, pipelines, and stored provenance of the deployment.
The operator runs the conversion as the steps of the revision `2026-10-v04`; the steps and their
place in the revision history are listed under
[2026-10-v04](../deploy/upgrades.md#2026-10-v04), and this page is the reference for how they
behave. A deployment at `2026-10-v02` also runs the two steps of the revision before it,
`2026-10-v03`, in the same upgrade (see [The steps](#the-steps)). The general upgrade procedure
(setting a target revision, approvals, and how steps run) is in
[Upgrading Thorium](../deploy/upgrades.md). A fresh install needs none of this.

## What changes for existing data

| Data | What the upgrade does |
| ---- | --------------------- |
| Images | Each image becomes a **tool** with the image's name, in the image's group, and a first **tool instance** holding the image's settings. The tool and instance ids derive from the group and the image name, so the same image always produces the same ids. The tool carries a `MigratedFromImage` tag naming the image, and its first instance is labeled with the image's own version, or `migrated` when the image has none. The image itself is left in place, read-only. |
| Pipelines | Each stage that names an image is pinned to the first tool instance of the tool migrated from that image (the pipeline's `stage_tools`), so the pipeline keeps running exactly what the image ran. A stage follows the tool's latest version only after its owner points it at the tool with `thorctl pipelines stage-tool`, or moves it to a newer instance with `--pin`. |
| Provenance | The tool names stored on filesystems, decompiled functions, and process trees move to `legacy_tools`. Names are never matched to tool instances. |
| Reactions in flight | **Nothing is deleted.** The upgrade waits, before it pauses anything, until no reaction is `Created` or `Started`. You let them finish or delete the stuck ones yourself. Finished reactions are kept and read as before. |
| Network policies | **Not changed** in the database. The scaler renders every policy again when it starts: an `allowed_tools` name selects the tools in the policy's group with exactly that name by their ids, and an id selects the tool or tool instance with that id (see [Tool Entries](../network_policies/network_policies.md#tool-entries)). A migrated tool keeps its image's name, so a name that selected an image's workers selects the migrated tool's. A tool created or renamed later with the same name is selected too, so replace name entries that guard a security boundary with tool ids. Worker pods started before the upgrade carry no `thorium-tool` label, so name and tool-id entries don't select them until they're replaced (tool-instance-id entries still do). |
| Results | **Not changed.** A result keeps the tool name it was stored with. A migrated tool's results are found by name, so a tool renamed after the upgrade stops matching its old results. |

After the upgrade:

- Image writes (`POST /api/images/`, `PATCH /api/images/{group}/{image}`, and the Thorctl commands
  built on them) return `410 Gone` with a pointer to tools. Image reads, lists, and deletes keep
  working. `thorctl toolbox import` imports tools instead: it reads the repository's toolbox
  (`tools/toolbox.json`, now made of tools) and converts a legacy image toolbox to tools as it reads
  it, and `thorctl toolbox convert` writes the converted file (see [Toolboxes](../../developers/toolbox.md#converting-a-legacy-toolbox)). The chart's toolbox
  import Job works again; importing the repository's toolbox needs an admin login.
- An image the upgrade could not convert has no tool. A pipeline with a stage that uses one is left
  untouched as a whole, and reactions on it are refused with `400` until every stage has a tool
  (see [Pipelines the upgrade leaves alone](#pipelines-the-upgrade-leaves-alone)).
- An image whose environment variable names look like secrets (names containing `KEY`, `TOKEN`,
  `SECRET`, `PASSWORD`, `PASSWD`, or `CREDENTIAL`, in any case) is migrated unchanged, with a
  warning in the report. The variables of a tool instance are visible to every group that can read
  the tool, and image writes are refused once the upgrade is done, so move such values to
  Kubernetes `Secret` volumes **before** you upgrade.

## Before you upgrade

1. Read the [release notes](../../release_notes/agentic_workflows.md), including the known gaps.
2. Plan for the outage: [the API, the scalers, the event handler, and the search streamer are down
   while the conversion steps run](#the-outage).
3. Let the system drain: stop what creates reactions (triggers, bulk submissions, and users), and
   let running reactions finish. The upgrade deletes nothing: it waits while any reaction is in
   flight (see [The drain](#the-drain)), and the wait never ends if something keeps submitting
   reactions.
4. Clean up images with secret-looking `env` names (see above), and fix what the checks report
   while image writes still work.
5. Plan to run every step's check against the running cluster and read the reports (see
   [Checks and reports](#checks-and-reports)). A check stores its result in a table that
   `schema-apply` creates, so it can run only after that step: run the checks while the upgrade
   waits at `drain-reactions` or at the first conversion step's approval, when Thorium is still
   serving and image writes still work.
6. Take a backup of Redis, Scylla, and S3 with `thoradm backup new` and keep the
   `thorium-credentials` Secret and your values files (see
   [Taking Backups](../thoradm/taking_backups.md) and [Backups](../deploy/operate.md#backups)).
   The backup is the only way back, and every approval names it. It holds model credential secrets
   in plaintext, so store it accordingly.
7. Decide whether to approve every step up front (recommended, see
   [Approving the steps](#approving-the-steps)).

## The steps

The conversion is the five steps of `2026-10-v04`. A deployment at `2026-10-v02` runs the two steps
of `2026-10-v03` (`schema-apply` and `users-settings-theme`) in the same upgrade, so the table lists
all seven in the order that upgrade runs them. Two paths reach `2026-10-v04`, and both work:

- **`2026-10-v02` to `2026-10-v04` directly**, with an image that knows `2026-10-v04`: all seven
  steps run in the order below.
- **`2026-10-v02` to `2026-10-v03`, later `2026-10-v03` to `2026-10-v04`**: an image that only
  knows `2026-10-v03` runs the first and third rows and records `2026-10-v03`. When the deployment
  is upgraded to this release, the five steps of `2026-10-v04` run, because the recorded
  `2026-10-v03` doesn't hold them. Nothing of the conversion is skipped, and the drain is the first
  step.

Each step is recorded in the `thorium-upgrade-state` ConfigMap and in Scylla, so an interrupted
upgrade resumes at the first unfinished step. `thoradm migrate plan --from 2026-10-v02` prints the
seven steps and `thoradm migrate plan --from 2026-10-v03` the five of `2026-10-v04`.

| Step | Kind | Pauses | Needs an approval | What it does |
| ---- | ---- | ------ | ----------------- | ------------ |
| `schema-apply` | Infra | Nothing | No | Revision `2026-10-v03`. Applies the schema this release declares and records what was applied. It runs first, so every later step finds the tables and columns it reads. |
| `drain-reactions` | Data | Nothing | No | Deletes nothing. Its check waits, with Thorium running, while reactions are `Created` or `Started` or workers of the previous release are registered, and the step is `Skipped` when none are. It runs before any component is paused, and ahead of `users-settings-theme` when one upgrade runs both. |
| `users-settings-theme` | Data | API | When a user still holds its theme in its own field | Revision `2026-10-v03`. Moves each user's theme into the user's `settings`. Only users written by a development image have the old layout, so a deployment of a released version finds every user converted and records the step `Skipped`. |
| `images-to-tools` | Data | API, scaler, baremetal-scaler, event handler, search streamer | When there is an image to migrate | Creates one tool and a first tool instance for each image. |
| `pipelines-to-tool-stages` | Data | The same | When there is a stage to convert | Pins each stage of every pipeline whose stages all have a migrated tool to that tool's first instance. Its check runs again after `images-to-tools` finishes. |
| `provenance-to-legacy-tools` | Data | The same | When a stored row has legacy names | Moves stored provenance names to `legacy_tools`. |
| `finalize-tool-only` | Config | Nothing | No | Marks the conversion finished. The operator then records the revision. |

A `Data` step with nothing to do is recorded `Skipped` and asks for no approval. `drain-reactions`
has no migration to run and never asks for an approval (`thoradm migrate list` marks it like the other
Data steps); an approval that names it is reported as stale. Once every step is done or skipped, the operator applies the new release's Deployments and
only then scales each paused component back to the replica count it had before the upgrade, one
component at a time.

### The drain

`drain-reactions` deletes nothing. An in-flight reaction has no plan, and its queued jobs and
worker records have the previous shape, so the new release can't continue it, and the upgrade
leaves the decision about unfinished work to you. The step runs **before any component is paused**
(first on a deployment at `2026-10-v03`, right after `schema-apply` on one at `2026-10-v02`, where
it also runs ahead of the `users-settings-theme` step that pauses the API), so Thorium keeps running
normally while it waits. Its check
reads Redis and writes nothing:

- Nothing in flight and no workers of the previous release: the step is `Skipped`.
- Reactions `Created` or `Started`, or workers of the previous release still registered: the step
  shows `Waiting`. Its reason gives the counts, for example `2 reaction(s) are still Created or
  Started and 1 stage-path worker(s) are still registered`, and the check runs again every 15
  seconds. The reason never contains a reaction id, name, or group. Workers exit on their own once
  they find no job, so after the last reaction ends a worker may stay registered for a while.

To let the wait end:

1. Stop whatever creates reactions (triggers, bulk submissions, and users).
2. Let running reactions finish, or delete the ones that are stuck while the previous API is still
   running. A reaction that names a missing image can never be scheduled, for example. Reactions
   are listed and deleted per group and pipeline. A status filter needs `--pipeline`, and `delete`
   removes at most 50 reactions unless you add `--no-limit`:

   ```bash
   thorctl pipelines get --groups <group>
   thorctl reactions get --group <group> --pipeline <pipeline> --status created
   thorctl reactions get --group <group> --pipeline <pipeline> --status started
   thorctl reactions delete --group <group> --pipeline <pipeline> --status created --no-limit
   thorctl reactions delete --group <group> --pipeline <pipeline> --status started --no-limit
   ```

   `delete` also takes reaction ids (`thorctl reactions delete --group <group> <id> <id>`).
   Without `--pipeline`, `--status` is refused with an invalid argument combination error.
3. Watch the step with `kubectl -n thorium describe thoriumcluster thorium`. It becomes `Skipped`
   when nothing is left, and the conversion steps follow. You can run the same check yourself:

   ```bash
   thoradm --cluster-conf thorium.yml migrate check --id drain-reactions
   ```

Keep the previous API running while the drain waits: it is what you delete stuck reactions with. The
operator changes no Deployment while a step waits, so pods that are running keep running, but don't
restart or delete API pods meanwhile.

A drain counts only within the attempt that ran it. If you stop the upgrade before it reaches
`2026-10-v04`, by clearing the target or setting it back to the revision the deployment is at, the
operator records an `upgrade-stopped` entry, and the next attempt drains again before the first
conversion step, since reactions may have been submitted in between. A conversion step that
finished before the stop stays finished.

## The outage

The conversion steps (`images-to-tools`, `pipelines-to-tool-stages`, and
`provenance-to-legacy-tools`) scale down the API, scaler, baremetal-scaler, event handler, and
search streamer, because the previous API would write data in the shape being replaced. The
components stay down until the new release's Deployments are applied, **including while a later
step waits for its approval, after a failed step, and after you clear the target mid-step**, until
you retry it, set the target again, or restore the backup.
Nothing is paused before the first conversion step starts running, so while that step waits for its
approval Thorium keeps serving. Once it is down, Thorium is unavailable: users can't reach the API,
and nothing is scheduled.

Approving every step up front limits the outage to the steps' run time. `drain-reactions` pauses
nothing, so Thorium keeps running while it waits for reactions to finish; the outage starts only
when the first conversion step begins. Each step's check reports an estimate of the time its
components stay down. Each paused component's replica count is recorded on the step's `Running`
history entry in `thorium-upgrade-state`, and `kubectl -n thorium get deploy -w` shows the
components go down and come back.

## Approving the steps

Every `Data` step except `drain-reactions` needs an approval naming your backup when it has work
to do (`users-settings-theme` only when a user still holds its theme in its own field), like any other
approval (see [Approvals](../deploy/upgrades.md#approvals)). Approvals may be set before the upgrade
starts. The recommended procedure is to run the checks, take the backup, and then set the target
revision and every approval in one `helm upgrade`, so no step waits:

```yaml
operator:
  cluster:
    upgrade:
      targetRevision: 2026-10-v04
      approvals:
        - step: images-to-tools
          backup: thoradm backup 2026-11-02 on /mnt/backups
        - step: pipelines-to-tool-stages
          backup: thoradm backup 2026-11-02 on /mnt/backups
        - step: provenance-to-legacy-tools
          backup: thoradm backup 2026-11-02 on /mnt/backups
```

The `backup` text is your reference to a backup you took yourself; the operator never takes one.
megathor takes the same list as
`thorium_upgrade_approvals`, and a `ThoriumCluster` without Helm as `spec.upgrade.approvals`
(with `accept_issues` where Helm has `acceptIssues`).

A step that is waiting for its approval reports `Waiting for an approval` with its check summary.
To approve a step that is already waiting, add its approval and run `helm upgrade` again. To resume
a failed step, change its approval's `backup` value (or its `acceptIssues`), which re-approves it,
and it continues from its checkpoints. Once the upgrade is done, `status.upgrade.stale_approvals`
lists each approval as not a step of the plan; remove them from your values.

## Checks and reports

Each `Data` step has a **check** that classifies what it would change and writes nothing (its only
write is its own result row). The operator runs it when the step is planned and copies the result
into the step's entry in `status.upgrade` and into the approval message, so
`kubectl -n thorium describe thoriumcluster thorium` shows it while the step waits:

```
5 of 7 objects need migrating (2 unrecognized, 0 with errors), 10 rows to rewrite, about 1 seconds
while the paused components are down. It pauses api, scaler, baremetal-scaler, event-handler,
search-streamer while it runs. Largest issue groups: invalid_value:dependencies.results.images
(unrecognized, 1); missing_field:image (unrecognized, 1). Waiting for an approval: ...
```

The check of `pipelines-to-tool-stages` runs again once `images-to-tools` has finished, because
which stages convert depends on which images did. Its summary before and after can differ, and
while a check is running the step's message shows `Checking what step <id> would change` or
`Reading the report of the check Job` in place of the summary until it ends.

You can run the same check yourself against the running cluster once `schema-apply` has run, while
the upgrade waits at `drain-reactions` or at the first conversion step's approval and image writes
still work. Before `schema-apply`, the command fails with `unconfigured table
schema_migration_issues` because the table that holds a check's result doesn't exist yet.
`--cluster-conf` is an option of `thoradm` itself and goes before `migrate` (see
[Admin Command Line Tool](../thoradm/thoradm.md#config)):

```bash
thoradm --cluster-conf thorium.yml migrate check --id images-to-tools
thoradm --cluster-conf thorium.yml migrate check --id pipelines-to-tool-stages
thoradm --cluster-conf thorium.yml migrate check --id provenance-to-legacy-tools
thoradm --cluster-conf thorium.yml migrate check --id drain-reactions
```

A check's log in its Job holds only a short JSON result: the object counts (`current`, `migratable`,
`unrecognized`, `error`), the rows it would rewrite, an estimate in seconds, and the issue groups by
reason code. To read everything the check found, run it yourself with `--all --samples`, which
prints every issue group with up to 20 sample keys to your terminal only (never into a stored
result):

```bash
thoradm --cluster-conf thorium.yml migrate check --id images-to-tools --all --samples
```

That output also holds the report: the counts per group (changed, already migrated, unchanged, and
skipped), every skipped item with its reason, and notes for each item, including who each tool
would be published as (the image's creator while that user still exists and is in the image's
group, otherwise the account `thorium-migration`), and the label of its first version. Warnings are
the notes to act on before you approve: images whose `env` names look like secrets, and the
dependency names dropped because they name no migrated image. Reports name `env` variables and
never their values. The `drain-reactions` check reports no counts or issues, only whether it would
wait (and why) or skip.

After a step has run, what its last pass applied is stored with the step's run row:

```bash
thoradm --cluster-conf thorium.yml migrate report --id images-to-tools
```

The report has the same fields, for example `{"groups":{"images-to-tools":{"changed":6,
"already_migrated":0,"unchanged":0,"skipped":2}},"skipped_total":2,"skipped":[{"group":"...",
"item":"bare","reason":"missing_field:image"}], ...}`. `report` fails until a pass has ended. After
the upgrade, the `images-to-tools`, `pipelines-to-tool-stages`, and `provenance-to-legacy-tools`
checks report nothing left to migrate (objects they could not convert stay listed as issues). The
`drain-reactions` check is not about data: it reports a wait for as long as any reaction is Created
or Started, including reactions created after the upgrade.

## Issues

An object a step can't convert is left untouched and recorded as an **issue** under a **reason
code**. Issues are grouped by code, and a step finishes `Done` with issues as long as no group is
larger than its `group_max`; `status.upgrade` then shows a `MigrationIssues` condition naming the
largest groups. Every converting step of this revision uses a `group_max` of 5. A group of more than
5 objects fails the step, held `Failed`, unless the approval's `acceptIssues` names its code.

The reason codes this revision records:

| Code | Step | Meaning and remedy |
| ---- | ---- | ------------------ |
| `missing_field:image` | `images-to-tools` | The image has no container image reference, so it can't become a tool. Create the tool yourself with `thorctl tools create` (see below). |
| `invalid_value:dependencies.<kind>.images` | `images-to-tools` | Every name in one of the image's non-empty dependency lists (`results`, `children`, or `filesystems`) names no migrated image, so the image is skipped. Fix the list, or create the tool yourself. |
| `invalid_value:name` | `images-to-tools` | A tool with the image's name already exists and wasn't migrated from an image. Rename one of them and run the step again. |
| `missing_field:tool_instance` | `images-to-tools` | A tool made by an earlier run is missing its first instance. Run the step again. |
| `malformed_json` | `images-to-tools` | A stored image can't be parsed. It is left untouched. |
| `write_failed:<kind>` | `images-to-tools` | The tool was converted but its write was refused. |
| `invalid_value:stages.unresolved` | `pipelines-to-tool-stages` | A stage of the pipeline has no migrated tool, so the whole pipeline is left untouched (see below). |
| `malformed_json:order`, `malformed_json:stage_tools`, `missing_field:order` | `pipelines-to-tool-stages` | A stored pipeline can't be parsed. It is left untouched. |
| `malformed_json:kind_data` | `provenance-to-legacy-tools` | A stored provenance row can't be parsed. It is left untouched. |

A dependency name that names no migrated image, in a list that has other names that do, is dropped
from the migrated tool and noted in the report; it is not an issue. Reason codes never contain an
image, pipeline, or group name, or any stored value. List a step's issue groups, or one group's
keys, with:

```bash
thoradm --cluster-conf thorium.yml migrate issues --id images-to-tools
thoradm --cluster-conf thorium.yml migrate issues --id images-to-tools --reason missing_field:image
```

`issues` shows the groups of the step's current pass: the code, class, count, how many are
resolved, and whether the group was accepted. To handle a group, either fix the objects and run the
step again for them (`thoradm migrate run --id <id> --reason <code>`, or `--objects <key>,<key>`),
or accept the code for the objects you will leave:

```yaml
approvals:
  - step: images-to-tools
    backup: thoradm backup 2026-11-02 on /mnt/backups
    acceptIssues:
      - missing_field:image
```

An acceptance is used by the run it was given for. A later run of the step does not reuse it. The
`MigrationIssues` condition lists every step of the upgrade that ended with issues, the step that
ran last first, and reads `finished with no issues` only when no step has any; run
`thoradm migrate issues --id <step>` for a step's groups.

### Pipelines the upgrade leaves alone

A pipeline converts as a whole or not at all. When any stage of a pipeline uses an image that was
not migrated, `pipelines-to-tool-stages` records `invalid_value:stages.unresolved` for the pipeline
and writes nothing to it, so none of its stages has a tool, including the stages whose images did
migrate. The pipeline can't run (reactions on it are refused with `400`) until every stage has a
tool. For each unconverted image:

1. Create the tool from the image's settings with `thorctl tools create --spec <spec> --name <name>
   --group <group> --version <version>` (see
   [Tools and Tool Instances](../../developers/tools.md#an-image-tool)). Naming it exactly after the
   image keeps network policy `allowed_tools` rules working.
2. Set the tool of **every** stage of the pipeline, the stages that would have converted included:

   ```bash
   thorctl pipelines stage-tool <pipeline> <stage> <tool> --group <group>
   ```

Do not run the migration again to fix this. It only accepts the tool it created itself, with an id
derived from the image, and a tool made with `thorctl tools create` has a different one, so a rerun
doesn't set the stages and isn't needed. `thorctl pipelines describe --format json <group>/<pipeline>`
shows the pipeline's `stage_tools`; once it names a tool for every stage, reactions run again.

## Running a step by hand

Every `Data` step except `drain-reactions` (which has nothing to run) can be run by hand with
thoradm instead of by the operator, for example to run it again for objects you fixed. Run it from
a `thoradm` of the new version that has the cluster config (see
[Admin Command Line Tool](../thoradm/thoradm.md#config)):

```bash
thoradm --cluster-conf thorium.yml migrate run --id images-to-tools
```

The command takes the namespace's writer lease, so it is refused while a migration Job of the
operator holds it, and it prints a JSON result with the pass's status (`done`, or `done_with_issues`),
counts, and issue groups. Stop the components the step lists (see `thoradm migrate list`) first,
since the operator pauses them only for its own runs. When the operator next checks the step it
finds nothing left to migrate and skips it. `thoradm migrate run --id drain-reactions` and
`verify --id drain-reactions` print the manual procedure and exit with code 3. See
[Migrate](../thoradm/thoradm.md#migrate) for every flag and the exit codes.

## Interruptions and reruns

A step's run keeps a checkpoint after every batch. If its pod is killed or evicted, or the
connection to a backend is lost, the Job retries with backoff and resumes from the checkpoints
instead of starting over: a pod killed during a run of the provenance conversion was replaced by one
that reported `resumed: true` and wrote only the rows the first had not, and the run finished. An
operator that restarts while a step runs re-attaches to its Job, or starts a Job that resumes if it
is gone. A run that fails on issue groups over the limit, or fails the same way every time, is not
retried: the step is `Failed` until you re-approve it (see
[Approving the steps](#approving-the-steps)).

Clearing the target, setting it to the recorded revision (such as `2026-10-v03`), or setting a
target the operator can't use while a conversion runs stops its Job: the operator deletes the Job in
the foreground, the pod logs `signal received; finishing the batch and writing the checkpoint` and
exits once the checkpoint is written, and the cluster's message names the step, its Job, the paused
components with their counts, and the two ways forward. The components stay at zero; don't scale
them up by hand, since the previous release would run on partly converted data. Set the target to
`2026-10-v04` again and the upgrade resumes: the stopped step resumes from its checkpoints under the
same approval, without pausing anything again. After a cleared target or one set to the recorded
revision the drain runs again first, since those end the attempt (see [How steps
run](../deploy/upgrades.md#how-steps-run)); a target the operator can't use doesn't, so the drain
isn't run again. Or restore the backup, as in [Rolling back](#rolling-back).

A step can be run again at any time. Objects that are already migrated are counted as current and
written again as nothing: a second run of `images-to-tools` after it finished read 8 objects, wrote
0, and reported the same two issue groups.

## Rolling back

The conversion can't be undone, so Thorium refuses to revert across it. The catalog has a reverse
edge from `2026-10-v04` to `2026-10-v03`, but `images-to-tools`, `pipelines-to-tool-stages`, and
`provenance-to-legacy-tools` are declared irreversible: `thoradm migrate revert --to 2026-10-v03`
(with or without `--dry-run`), `thoradm migrate plan --from 2026-10-v04 --to 2026-10-v03`, and an
operator target below `2026-10-v04` fail before anything runs, naming the step (`Step images-to-tools
of revision 2026-10-v04 can't be undone, so this downgrade is refused before anything runs; restore
from backup instead (a backup taken before step images-to-tools ran)`). The same refusal stands for
a target of `2026-10-v02`. A step that finished as `Skipped` (it found nothing to convert) counts as
finished, because a cluster at `2026-10-v04` writes tools and pipelines in the new shapes whether or
not those steps converted anything. A cluster that reached `2026-10-v04` therefore can't be
reverted, even when the conversion steps found nothing to convert. A cluster whose upgrade stopped
partway is still recorded at `2026-10-v03`, but once a conversion step has a `Done` or `Skipped`
entry in its history, or a run that started (and was stopped or failed), a target of `2026-10-v02`
is refused the same way, naming that step, because the way down from `2026-10-v03` never undoes
it. Only a cluster that stopped before those steps ran (it only drained, at most) can still be
reverted: at `2026-10-v03` it holds no conversion step and can go back to `2026-10-v02`, where the
one step that runs, `users-settings-theme-reverse`, loses nothing (see
[2026-10-v03](../deploy/upgrades.md#2026-10-v03)). An older operator also refuses a
cluster at a revision it doesn't know. The reports are a record for audit and for fixing individual groups by
hand, not an undo. Rolling back is restoring the backup you named in the approvals:

1. Scale the operator and every Thorium component but the API to 0: the restore loads Redis through
   the API, so it stays up (see [Running a Restore](../thoradm/thoradm.md#running-a-restore)). After
   a conversion step started, the API is paused at 0 too: once the operator is at 0, scale the API
   to 1 for the restore (`kubectl -n thorium scale deployment api --replicas=1`). This is part of
   the restore only; the previous chart's operator sets every component's count in step 3. The
   paused API Deployment still runs the previous release, since the new release's Deployments are
   applied only once every step finishes, so it runs on partly converted data, but only until the
   `--wipe` restore in step 2 replaces that data.
   Delete the chart's toolbox import Job too if one is still running or retrying
   (`kubectl -n thorium get jobs -o name | grep toolbox-import`), since it would write tools into
   the namespace while the API is up.
2. Restore Scylla, Redis, and S3 from the backup with `thoradm backup restore` into the cluster you
   upgraded, with `--wipe <namespace>` since its tables are no longer empty. Use the previous
   release's `thoradm` when the upgrade stopped before it finished: the API it talks to is still
   the previous release, and a `--wipe` restore is refused when thoradm's version isn't the
   API's (see [Restoring a Backup](../thoradm/thoradm.md#restoring-a-backup) and
   [Merge or Wipe](../thoradm/thoradm.md#merge-or-wipe)). A backup that holds every Scylla table and
   Redis writes its schema record, revision included, back with the data.
3. Delete the `thorium-upgrade-state` ConfigMap, then deploy the previous chart version. Its operator
   rebuilds the ConfigMap from the restored record and reconciles the cluster at its own revision.
   With the newer release's ConfigMap still in place it refuses with `AheadOfCatalog` and never
   reads the restored record:

   ```bash
   kubectl -n thorium delete configmap thorium-upgrade-state
   ```

If you restored the data some other way, the recorded revision stays ahead of the data and the
previous operator refuses with `AheadOfCatalog`. Run
`thoradm --cluster-conf thorium.yml migrate reset-state --to <previous revision> --i-restored-data`
before step 3. It takes the writer lease, appends an audit entry, and voids the migration work of
every revision above the one you name.

A restore starts clean: reactions, jobs, workers, and run tokens are not in a backup (see
[What Is Backed Up](../thoradm/taking_backups.md#what-is-backed-up)), and anything created after the
backup, including tools and results created after the upgrade, is lost.

## After the upgrade

- Check the deployment: `kubectl -n thorium get thoriumcluster -o wide` shows the phase `Ready` at
  revision `2026-10-v04`, and `kubectl -n thorium get deploy` shows every component at its previous
  replica count.
- Check the tools: `thorctl tools get` lists the migrated tools, whose latest version is the
  image's version or `migrated`. `thorctl tools get -o json` includes each tool's tags, and a
  migrated tool has a `MigratedFromImage` tag naming its image. Compare the count with the report
  of `images-to-tools`, and tell group owners which images were skipped (see
  [Issues](#issues)).
- Check the pipelines: `thorctl pipelines describe --format json <group>/<pipeline>` lists a tool
  for every stage of a converted pipeline. Run a reaction on a converted pipeline with
  `thorctl reactions create --pipelines <group>/<pipeline>` and watch it finish.
- Check the network policies: `kubectl -n <group> get networkpolicy -o yaml` shows a
  `thorium-tool: <tool id>` peer for each tool-name entry, naming the migrated tool. A
  `thorium-tool: none` peer means no tool in that group has exactly that name.
- Remove the approvals from your values; they are reported as stale once the steps are done.
- Tell users that images are read-only and tools replace them, and that the Web UI does not yet
  support tools (see the known gaps in [Tools, Runs, and Models for Admins](./agentic.md)).
- Give migrated tools an owner's attention over time: the first tool instance keeps the image's
  settings, including its `env`, until a new instance is published.

## Troubleshooting

These behaviors were seen during an upgrade rehearsal and may appear on your deployment.

- **The status message of a step changes between a summary and a short line** such as `Reading the
  report of the check Job migration-check-...`. A check runs again when the state it reads changed,
  and the message shows that line until the new report is read. It is not a failure; the summary
  returns.
- **A `SchemaDrift` condition is `True` while a step is blocked.** In the rehearsal it named the
  Scylla tables `schema_migrations` and `schema_migration_issues` as unexpected and the Redis
  `cursors` object as changed, and was `False` again once the upgrade finished. See
  [Comparing the Live Schema](../thoradm/thoradm.md#comparing-the-live-schema) to read the drift;
  it does not stop the steps.
- **`MigrationIssues` stays `True` after a later step finished clean.** It lists every step that
  ended with issues, so the issues of `images-to-tools` stay named after `provenance-to-legacy-tools`
  finishes with none. List a step's groups with `thoradm migrate issues --id <step>`.
- **Pipelines refuse reactions with `has stages with no tool` after a backup from before revisions
  was restored.** A backup without `schema.json` holds images and pipelines that were never
  converted. `thoradm backup restore` of a whole such backup records `2026-10-v02` (see [The Schema
  Record](../thoradm/thoradm.md#the-schema-record)), so the operator plans this upgrade's steps
  again once it reads the record; approve them as above. Data restored some other way leaves the
  record at `2026-10-v04`: run `thoradm migrate reset-state --to 2026-10-v02 --i-restored-data`,
  then delete the `thorium-upgrade-state` ConfigMap so the operator reads the record at its next
  reconcile (see [Resetting the Recorded
  State](../thoradm/thoradm.md#resetting-the-recorded-state)).
