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

The release's newest revision is `2026-10-v05`, which follows `2026-10-v04` and adds the yara
scanning schema: the `scan_stats` table, the `entities_by_name` view for exact-name entity reads,
and the bucket compiled yara rulesets are cached in (`thorium.yara.cache_bucket`,
`thorium-yara-rulesets` by default). Its one step, `finalize-yara-scanning` (Config), records the
revision and changes no data; the operator then applies the image's schema (the table and the
view) and creates the bucket as it reconciles the new image (see
[2026-10-v05](../deploy/upgrades.md#2026-10-v05)). A target of `2026-10-v05` runs the conversion
and then that step in one upgrade; a target of `2026-10-v04` stops after the conversion, and
`2026-10-v05` can be targeted later.

## What changes for existing data

| Data | What the upgrade does |
| ---- | --------------------- |
| Images | Each image becomes a **tool** with the image's name, in the image's group, and a first **tool instance** holding the image's settings. The tool and instance ids derive from the group and the image name, so the same image always produces the same ids. The tool carries a `MigratedFromImage` tag naming the image, and its first instance is labeled with the image's own version, or `migrated` when the image has none. A banned image's first instance carries the image's bans, so it doesn't run until an admin lifts them (see [Tool Instance Bans](../bans_admins.md#tool-instance-bans)). The image itself is left in place, read-only. |
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
| `drain-reactions` | Data | Nothing while it waits; with a cancel approval, scaler and baremetal-scaler first, then the API, event handler, and search streamer too | No; an approval with `cancelInFlight` and the step's confirmation code cancels instead of waiting | Its check waits, with Thorium running, while reactions are `Created` or `Started` or workers of the previous release are registered, and the step is `Skipped` when none are. With a cancel approval it fails the reactions still in flight, records each request, and the requests are resubmitted once the conversion is recorded (see [The drain](#the-drain)). It runs before any component is paused, and ahead of `users-settings-theme` when one upgrade runs both. |
| `users-settings-theme` | Data | API | When a user still holds its theme in its own field | Revision `2026-10-v03`. Moves each user's theme into the user's `settings`. Only users written by a development image have the old layout, so a deployment of a released version finds every user converted and records the step `Skipped`. |
| `images-to-tools` | Data | API, scaler, baremetal-scaler, event handler, search streamer | When there is an image to migrate | Creates one tool and a first tool instance for each image. |
| `pipelines-to-tool-stages` | Data | The same | When there is a stage to convert | Pins each stage of every pipeline whose stages all have a migrated tool to that tool's first instance. Its check runs again after `images-to-tools` finishes. |
| `provenance-to-legacy-tools` | Data | The same | When a stored row has legacy names | Moves stored provenance names to `legacy_tools`. |
| `finalize-tool-only` | Config | Nothing | No | Marks the conversion finished. The operator then records the revision. |

A `Data` step with nothing to do is recorded `Skipped` and asks for no approval. `drain-reactions`
never asks for an approval: it waits until nothing is in flight. An approval that names it opts in
to cancelling what is in flight instead, and counts only with a backup, `cancelInFlight: true`, and
the confirmation code the waiting step shows (see [Cancelling instead of
waiting](#cancelling-instead-of-waiting)). While the step is planned, an approval naming it that
doesn't count is listed in `status.upgrade.stale_approvals` with the reason and the current code;
once the step is done, any approval naming it is listed there as not a step of the plan. Once every
step is done or skipped, the operator applies the new release's Deployments and
only then scales each paused component back to the replica count it had before the upgrade, one
component at a time.

### The drain

Unless you approve a cancel, `drain-reactions` deletes nothing. An in-flight reaction has no plan,
and its queued jobs and worker records have the previous shape, so the new release can't continue
it, and the upgrade leaves the decision about unfinished work to you: wait for it, delete it, or
approve the cancel described in [Cancelling instead of waiting](#cancelling-instead-of-waiting). The
step runs **before any component is paused**
(first on a deployment at `2026-10-v03`, right after `schema-apply` on one at `2026-10-v02`, where
it also runs ahead of the `users-settings-theme` step that pauses the API), so Thorium keeps running
normally while it waits. Its check
reads Redis and Scylla and writes nothing:

- Nothing in flight and no workers of the previous release: the step is `Skipped`.
- Reactions `Created` or `Started`, or workers of the previous release still registered: the step
  shows `Waiting`. Its reason gives the counts, for example `2 reaction(s) are still Created or
  Started and 1 stage-path worker(s) are still registered`, points to
  `thoradm migrate check --id drain-reactions`, and the check runs again every 15 seconds. The
  reason never contains a reaction id, name, or group. Workers exit on their own once they find no
  job, so after the last reaction ends a worker may stay registered for a while. The reason ends
  with the approval that would cancel instead, including the step's confirmation code, for example
  `To cancel what is in flight instead, take a backup and add the approval {step: drain-reactions,
  backup: <where your backup is>, cancelInFlight: true, confirm: 7KQ2M9XA} (Helm value
  operator.cluster.upgrade.approvals[]; cancel_in_flight: true in the ThoriumCluster), repeating the
  code the step shows as confirm`.

To see what holds the drain, run its check with the cluster config:

```bash
thoradm --cluster-conf thorium.yml migrate check --id drain-reactions
```

It prints the JSON report on stdout, as every check does, and names each reaction and worker in two
tables on stderr. The tables reach your terminal or the check Job's log, never the stored report or
the cluster status. The operator deletes each check Job once it has read the report, so run the
check yourself to see the tables. Each table shows at most 50 rows, orphans and workers on
unregistered nodes first, and ends with a line such as
`... and 120 more reaction(s) not shown, 3 of them orphans` when there are more. The exit code is 0,
as for any check that asks to wait:

```text
$ thoradm --cluster-conf thorium.yml migrate check --id drain-reactions
drain-reactions waits: 2 reaction(s) Created or Started, 2 stage-path worker(s) registered

Reactions holding the drain
ID                                    GROUP  PIPELINE  STATUS   AGE     LAST ACTIVITY               LISTED
5e0f8a90-1d3b-4f0e-8c6a-7b2d9e4f3c10  wheat  strings   Created  -       2026-10-29T08:00:03Z (sla)  no (orphan)
0b6d1c3e-6f2a-4c55-9a51-2f4c3e8d1a77  corn   unpack    Started  17h26m  2026-11-04T10:02:11Z        yes

Stage-path workers
NAME               SCALER  CLUSTER  NODE    STATUS   HEARTBEAT             NODE REGISTERED
corn-unpack-a41d0  K8s     prod     node-9  Running  -                     no
corn-unpack-7f9c2  K8s     prod     node-3  Running  2026-11-04T10:02:40Z  yes
{"migration":"drain-reactions","counts":{...},"families":["reactions","workers"],...,"wait":{"reason":"2 reaction(s) are still Created or Started and 2 stage-path worker(s) are still registered. The conversion waits until none remain, with Thorium running normally. See which ones with `thoradm migrate check --id drain-reactions`; ...","retry_after_secs":15},...}
```

- **AGE** is the time since the reaction's first status log entry (the reaction holds no creation
  time), and **LAST ACTIVITY** is its newest entry. A reaction without a status log shows `-` and
  its SLA deadline, marked `(sla)`.
- **LISTED** says whether the reaction is in its pipeline's set for its status. A reaction marked
  `no (orphan)` is missed by the status listings, so `thorctl reactions get --status` and
  `thorctl reactions delete --status` won't find it. `thorctl reactions get --group <group>
  --pipeline <pipeline>` without `--status` may still list it; delete it by id.
- **NODE REGISTERED** says whether the worker's cluster and node are among the API's registered
  nodes. A worker whose node isn't registered may be a leftover registration rather than a
  running worker.

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

   `delete` also takes reaction ids (`thorctl reactions delete --group <group> <id> <id>`), which
   is how an orphan from the named report is deleted. Without `--pipeline`, `--status` is refused
   with an invalid argument combination error.
3. Watch the step with `kubectl -n thorium describe thoriumcluster thorium`. It becomes `Skipped`
   when nothing is left, and the conversion steps follow. Run the check above again to see what
   is still left.

Keep the previous API running while the drain waits: it is what you delete stuck reactions with. The
operator changes no Deployment while a step waits, so pods that are running keep running, but don't
restart or delete API pods meanwhile.

A drain counts only within the attempt that ran it. If you stop the upgrade before it reaches
`2026-10-v04`, by clearing the target or setting it back to the revision the deployment is at, the
operator records an `upgrade-stopped` entry, and the next attempt drains again before the first
conversion step, since reactions may have been submitted in between. A conversion step that
finished before the stop stays finished.

#### Cancelling instead of waiting

When the reactions in flight can't be waited for (a long hunt, a reaction stuck on a missing image,
or more than you want to delete one by one), approve the drain with `cancelInFlight`. The step then
fails what is still in flight, records each reaction's request, and resubmits the requests as their
creators once the conversion is recorded. Results the cancelled reactions already wrote are kept,
and a resubmitted reaction runs every stage of its pipeline again, the stages that had finished
included.

1. Run `thoradm --cluster-conf thorium.yml migrate check --id drain-reactions` and read the named
   tables, so you know what will be cancelled.
2. Take a backup (see [Before you upgrade](#before-you-upgrade)). The approval names it, as every
   approval does.
3. Read the confirmation code the waiting step shows. It is in the step's message and in its
   `blocked` field (`{"CancelApproval":{"confirm":"7KQ2M9XA"}}`), eight characters computed from
   the upgrade history. The code changes when the history gains an entry, and the drain's run
   records one, but nothing is recorded while the drain waits: an approval written before the step
   waits, or left from an attempt stopped before the drain started, can still match. Add the
   approval only when you mean to cancel, and remove it once the drain has run:

   ```bash
   kubectl -n thorium get thoriumcluster thorium -o jsonpath='{.status.upgrade.steps[0].message}'
   ```

4. Add the approval and run `helm upgrade` again. Approving the conversion steps in the same
   `helm upgrade` keeps the outage short, since the components the cancel pauses stay down until
   the new release's Deployments are applied:

   ```yaml
   operator:
     cluster:
       upgrade:
         approvals:
           - step: drain-reactions
             backup: thoradm backup 2026-11-04 on /mnt/backups
             cancelInFlight: true
             # the code the waiting step shows
             confirm: 7KQ2M9XA
             # optional: 60 when unset, 0 to 3600
             graceSeconds: 60
           - step: images-to-tools
             backup: thoradm backup 2026-11-04 on /mnt/backups
           # and the other conversion steps
   ```

   A `ThoriumCluster` without Helm takes the same approval in snake case under
   `spec.upgrade.approvals`: `cancel_in_flight: true`, `confirm`, and `grace_seconds`. megathor
   passes `thorium_upgrade_approvals` through as written in the Helm form.

An approval without `cancelInFlight: true` changes nothing: the step keeps waiting. An approval
whose `confirm` is missing or isn't the current code changes nothing either; the step's message ends
`the approval repeats no confirmation code; the current code is <code>` or `the approval's
confirmation code doesn't match; the current code is <code>`, and `status.upgrade.stale_approvals`
lists it. A code is used up when the step starts stopping the scalers and never matches again, so
**one approval serves one attempt**: if the attempt stops, the cluster is restored or its recorded
state reset, or the approval is withdrawn during the grace, the next attempt shows a new code and
waits for a new approval.

With a usable approval the operator runs these phases in order, whatever is left in flight:

| Phase | What happens | Step message |
| ----- | ------------ | ------------ |
| Quiesce | Records the scaler's and baremetal-scaler's replica counts in the upgrade history, then scales both to 0, so no new job is scheduled and no Deployment apply brings them back. The API, event handler, and search streamer keep running, so jobs that are running can finish and report | `Stopped scaler, baremetal-scaler for a grace of 60 seconds before step drain-reactions cancels what is in flight; 42 seconds left` |
| Grace | Waits `graceSeconds` (60 when unset, at most 3600) from the recorded quiesce, so jobs near their end can finish. An operator restart keeps the recorded start and counts | The same |
| Pause | Scales the rest of the conversion pause (API, event handler, search streamer) to 0, so nothing new can be submitted, and records their counts | The step's `Running` message names the migration Job |
| Cancel | The `drain-reactions` migration Job records each `Created` or `Started` reaction's request, deletes its queued jobs, fails it with the reason in its status log, and removes the previous release's worker registrations. With nothing left, it records nothing and finishes | The same |
| Worker pods | The operator deletes the previous release's worker pods the Job names, but only those in the operator's own Kubernetes cluster, in a group's namespace, carrying Thorium's worker labels, and on the recorded node; any other is counted as not reachable | `Done`, with the counts, for example `...; worker pods: 1 deleted, 0 not reachable; 0 workers not listed for pod deletion` |

The step is `Done` on this path, never `Skipped`, even when nothing was left after the grace. Its
history entry's `work` holds the counts: `<n> reactions cancelled; requests: <n> recorded, <n> kept
from an earlier run, <n> refused, <n> stale; <n> workers removed`. A cancel run retried after its
Job failed counts the `recorded` requests the failed run left as kept, and the resubmission creates
what either run cancelled. The cancel records a request `refused` when it can never be resubmitted
(arguments over 64 KiB, uploaded files or a cache, a creator that is a system account or holds no
owner, manager, or user role in the group, or a pipeline that is gone), and `stale` when it was idle
for more than 30 days. Its reaction is failed all the same, and its status log says it was not
resubmitted.

**Thorium is down from the pause until the rollout.** From the moment the API, event handler, and
search streamer are scaled to 0, Thorium is unavailable, as for any conversion step, and it stays
down through the conversions until the new release's Deployments are applied. The resubmission runs
inside that window, so users have no chance to change a pipeline or a group before their work is
created again.

**Withdrawing during the grace.** Removing the approval, removing its `cancelInFlight`, or clearing
the target before the cancel Job is created scales the scalers back to their recorded counts and
records that their pause ended; the step goes back to waiting, and a new approval needs the new
code. Once the cancel Job exists, the run owns the pause: a withdrawn approval stops it like any
started step, with every paused component left at 0 (see [Interruptions and
reruns](#interruptions-and-reruns)).

**The resubmission.** Once `finalize-tool-only` has recorded `2026-10-v04`, and before the paused
components come back, the operator runs a `resubmit-reactions` Job (`thoradm migrate run --id
resubmit-reactions`). It decides from the upgrade history in Scylla, so a lost state ConfigMap
doesn't lose it: a cancel run that recorded requests, with no finished resubmission after it and no
stop, restore, or state reset in between. A failed Job is run again, up to three Jobs inside the
pause; after that the paused components come back anyway, and the operator runs another Job only
when you set the `thorium.sandia.gov/refresh-check` annotation on the `ThoriumCluster` to a new
value. The `ReactionsResubmitted` condition reports progress and the outcome, with counts only, for
example `The requests the drain recorded were resubmitted: resubmitted 4, refused 1, stale 0`.

Each request is created again as its creator, with its samples, repos, tags, and arguments, plus the
tag `resubmitted-from:<old id>`, the pipeline's default SLA, and no parent. Before creating it, the
Job checks it as the create route would and more: the creator must still exist and hold the owner,
manager, or user role in the group, the pipeline must be converted and unchanged since the cancel,
and the arguments must still be stored (they expire after 14 days); a sub reaction of a recorded
parent is left for the parent's resubmission to create. [Cancelling and Resubmitting
Reactions](../thoradm/thoradm.md#cancelling-and-resubmitting-reactions) lists every reason. A
refused request keeps its reason and is never resubmitted, and a request recorded outside this
attempt is marked `stale`. A hand run with `--include-stale` takes stale requests through every
check, so it refuses one the conversion pause never bound to its stage tools (only the operator's
run inside the pause binds them); submit refused and stale requests again with `thorctl`. A Redis,
Scylla, or S3 error isn't a refusal: the request stays `recorded` and the next Job retries it. Each
request's new reaction id is reserved before the create, so a rerun never creates it twice.

The cancelled reaction stays `Failed` until its retention expires. Its status log holds `cancelled
by the upgrade to 2026-10-v04; recorded to be resubmitted once the upgrade finishes`, and once it is
resubmitted a second entry, `resubmitted by the upgrade to 2026-10-v04 as <new id>` (see [Reactions
an upgrade cancelled](../../users/spawning_reactions.md#reactions-an-upgrade-cancelled)).

The reports name what each side did; they never print arguments, only their hash and size:

```bash
# each reaction recorded and each worker registration removed
thoradm --cluster-conf thorium.yml migrate report --id drain-reactions
# each request with its outcome: the new reaction id, or why it was refused or is stale
thoradm --cluster-conf thorium.yml migrate report --id resubmit-reactions
```

Tell the creators of refused and stale requests: the report names each request's group, pipeline,
and creator.

#### Cancelling by hand

Outside the operator, `thoradm migrate run --id drain-reactions` cancels the reactions in flight:
it records each one's request, deletes its jobs, fails it, and removes the stage-path workers'
registrations. `thoradm migrate run --id resubmit-reactions` creates the recorded requests again as
their creators, and `thoradm migrate report --id drain-reactions` and `--id resubmit-reactions`
list what each did. [Cancelling and Resubmitting
Reactions](../thoradm/thoradm.md#cancelling-and-resubmitting-reactions) describes both, their
flags, and the reasons a request is refused. A cancel run by hand records its requests outside any
drain attempt, so nothing resubmits them; the report lists what to submit again.

#### Backups and restores while requests are recorded

The recorded requests are `drain_request`
rows in the schema record, and their arguments are rows that expire after 14 days. While any
`drain_request` row exists, take and restore backups with a thoradm of this release or newer. An
older thoradm copies the arguments rows into the backup with the rest of the record and writes
them back with no expiry on restore. A restore with this release merges the backup's
`drain_request` rows with the target's without moving any request's status backward, and never
restores arguments. If you restore a backup taken between the cancel and the resubmission, the
restore closes the drain attempt, so the operator doesn't resubmit those requests: run the
resubmission by hand once revision `2026-10-v04` is recorded again (see [After a
Restore](../thoradm/thoradm.md#after-a-restore)). A run without `--include-stale` only marks the
requests `stale`; `--include-stale` takes them, still through every check, so a request with
arguments, or one the conversion pause never bound to its stage tools, is refused and has to be
submitted again with `thorctl`:

```bash
thoradm --cluster-conf thorium.yml migrate run --id resubmit-reactions --include-stale
thoradm --cluster-conf thorium.yml migrate report --id resubmit-reactions
```

## The outage

The conversion steps (`images-to-tools`, `pipelines-to-tool-stages`, and
`provenance-to-legacy-tools`) scale down the API, scaler, baremetal-scaler, event handler, and
search streamer, because the previous API would write data in the shape being replaced. The
components stay down until the new release's Deployments are applied, **including while a later
step waits for its approval, after a failed step, and after you clear the target mid-step**, until
you retry it, set the target again, or restore the backup.
Nothing is paused before the first conversion step starts running, unless you approved the drain's
cancel, so while that step waits for its approval Thorium keeps serving. Once it is down, Thorium is unavailable: users can't reach the API,
and nothing is scheduled.

Approving every step up front limits the outage to the steps' run time. `drain-reactions` pauses
nothing while it waits, so Thorium keeps running while reactions finish; the outage starts only
when the first conversion step begins. An approved cancel starts it earlier: the scalers stop at
the start of the grace, and the API, event handler, and search streamer when the cancel begins (see
[Cancelling instead of waiting](#cancelling-instead-of-waiting)). The resubmission then runs before
the components come back, so it adds to the outage. Each step's check reports an estimate of the time its
components stay down. Each paused component's replica count is recorded on the step's `Running`
history entry in `thorium-upgrade-state`, and `kubectl -n thorium get deploy -w` shows the
components go down and come back.

## Approving the steps

Every `Data` step except `drain-reactions` needs an approval naming your backup when it has work
to do (`users-settings-theme` only when a user still holds its theme in its own field), like any other
approval (see [Approvals](../deploy/upgrades.md#approvals)). `drain-reactions` takes an approval
only to cancel instead of waiting, and that approval can't be set up front: it repeats a code the
step shows once it waits (see [Cancelling instead of waiting](#cancelling-instead-of-waiting)). Approvals may be set before the upgrade
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

Every `Data` step can be run by hand with thoradm instead of by the operator, for example to run it
again for objects you fixed. Run it from
a `thoradm` of the new version that has the cluster config (see
[Admin Command Line Tool](../thoradm/thoradm.md#config)):

```bash
thoradm --cluster-conf thorium.yml migrate run --id images-to-tools
```

The command takes the namespace's writer lease, so it is refused while a migration Job of the
operator holds it, and it prints a JSON result with the pass's status (`done`, or `done_with_issues`),
counts, and issue groups. Stop the components the step lists (see `thoradm migrate list`) first,
since the operator pauses them only for its own runs. When the operator next checks the step it
finds nothing left to migrate and skips it. `thoradm migrate run --id drain-reactions` cancels the
reactions in flight instead of waiting for them (see [Cancelling by hand](#cancelling-by-hand)). See
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
- Tell users that images are read-only and tools replace them, in the Web UI as in Thorctl, and
  which Web UI pages don't work yet (see the known gaps in [Tools, Runs, and Models for
  Admins](./agentic.md)).
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
