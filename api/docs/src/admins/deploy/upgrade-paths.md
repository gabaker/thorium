# Upgrade Paths

This page maps the routes from an existing Thorium deployment to the current release: where a
deployment can start, the order of the stages, what each stage changes, how to check it and roll
it back, and the problems long-lived deployments have run into on the way, with their fixes. The
details of each mechanism live on their own pages: [Upgrading Thorium](./upgrades.md) (revisions,
targets, approvals, drift, downgrades), [Converting a Pre-Helm Deployment](./convert-to-helm.md),
[Taking Backups](../thoradm/taking_backups.md), and
[Upgrading to Agentic Workflows](../agentic/upgrading.md).

Commands use the namespace `thorium`; use `<prefix>-thorium` with a [namespace
prefix](./concepts.md#namespaces-and-the-namespace-prefix). `$CHART` and `$VERSION` stand for the
chart reference and version you install (for example
`oci://ghcr.io/cisagov/thorium/charts/thorium` and a release version), and `<registry>/thorium:<tag>`
for the Thorium image a stage runs.

## Where a deployment can start

| Starting point | How to recognize it | Path |
|----------------|---------------------|------|
| A pre-Helm deployment (made by the minithor or megathor scripts, or by hand from the `k8s/` manifests of earlier releases) | A `ThoriumCluster` with `thorium.secret_key` inline in `spec.config` and no `config_secrets`; an `operator` Deployment outside any Helm release; no `thorium-upgrade-state` ConfigMap. A current operator sets the phase to `Error` with "has not been converted" | [The pre-Helm path](#the-pre-helm-path): stages 1, 2, and 3 |
| A deployment of the first Helm release | A Helm release owns the `ThoriumCluster`, but Scylla holds no schema record and there is no `thorium-upgrade-state` ConfigMap | Deploy the current chart with `operator.cluster.upgrade.startingRevision=2026-10-v02`, then set a target (see [Where a deployment starts](./upgrades.md#where-a-deployment-starts)) |
| A Helm deployment at a recorded revision | `kubectl -n thorium get thoriumcluster -o wide` shows a `REVISION` | [From a recorded revision](#from-a-recorded-revision) |

Find what a deployment runs and records:

```bash
# phase and revision of a Helm or converted deployment
kubectl -n thorium get thoriumcluster -o wide
kubectl -n thorium get configmap thorium-upgrade-state \
  -o jsonpath='{.metadata.labels.thorium\.sandia\.gov/state-revision}'
# the image each component runs (record the digests before you change anything)
kubectl -n thorium get deploy -o wide
kubectl -n thorium get pods -o jsonpath='{range .items[*]}{.metadata.name} {.status.containerStatuses[0].imageID}{"\n"}{end}'
# the version the API reports
curl -s <THORIUM_URL>/api/version
```

Releases built from different code can report the same Thorium version, so record image digests,
not only tags and versions. Always run the `thoradm` that matches the API you are working on: the
API serves it at `<THORIUM_URL>/api/binaries/linux/x86-64/thoradm`, and the API pod has it as
`/app/thoradm`. Its version check compares version numbers only, so it doesn't catch a thoradm from
a different build of the same version.

## The pre-Helm path

A long-lived pre-Helm deployment reaches the current release in three stages. Each stage can be
deployed, checked, and rolled back on its own, and each ends with a known-good state:

| Stage | Runs | What the deployment gains | Rollback point |
|-------|------|---------------------------|----------------|
| 1. Backup-capable release | A pre-Helm release whose thoradm writes verifiable backups (`thoradm backup verify` exists) and whose API backs up every Redis record | Complete backups with a manifest, checked against the cluster | The images of stage 0 (the images the deployment ran before stage 1); no data changes |
| 2. Helm conversion and schema revisions | The first release whose operator runs revision upgrades (its latest revision is `2026-10-v03`), converted with its `convert-to-helm.sh`, upgraded from `2026-10-v01` to `2026-10-v03` | Helm charts, the operator's revision upgrades, the declared Scylla schema and its record | The verified backup taken right before the conversion, restored into a fresh stage-1 install |
| 3. Follow-up release | A later release, upgraded with `helm upgrade` to `2026-10-v05` | Tools, workflows, runs, and models (images and pipelines converted), YARA scanning | The verified backup taken right before the upgrade, restored into a fresh stage-2 install |

The current release can also convert a pre-Helm deployment and upgrade it from `2026-10-v01` to
its latest revision in one go (the [upgrade paths table](./upgrades.md#upgrade-paths)); the staged
path costs more outages but gives a tested rollback point before each irreversible change. Either
way, the backup before the conversion has to come from a thoradm and API that back up everything
(stage 1 or later): an older API silently leaves out records it can't parse and some Redis data
(scoped tokens, network-policy sets), and an older thoradm writes backups that can't be verified.

### Stage 1: Move to a backup-capable release

**What changes.** Only the images. The operator and every component move to the stage-1 image;
no table, view, or setting changes, so the swap needs no data work.

**Before you start:**

- Record the current image digests and the version the API reports (see above).
- `thorium.namespace` holds only lowercase letters, digits, and underscores, starts with a letter,
  and is at most 48 characters. Newer services refuse any other namespace, and it can't be renamed
  (it names the Scylla keyspace and the Redis key prefix).
- Pull access to the stage-1 image in both places a pre-Helm deployment pulls from: the operator
  Deployment's image pull secret, and the `ThoriumCluster`'s `spec.registry_auth` entry for the
  registry, from which the operator renders the components' `registry-token` Secret.
- A host that reaches every Scylla node, the S3 endpoint, and the API, with private disk for about
  the size of the buckets plus the tables. A backup holds every password hash and token and isn't
  encrypted.
- A cluster config (`thorium.yml`, see [Config](../thoradm/thoradm.md#config)) and a Thorctl config
  logged in as an admin (`thorctl login`).

**Steps.**

1. Point the operator and the `ThoriumCluster` at the stage-1 image; the operator rolls every
   component when the `ThoriumCluster` changes:

   ```bash
   TC=$(kubectl -n thorium get thoriumcluster -o name)
   kubectl -n thorium set image deployment/operator operator=<registry>/thorium:<tag>
   kubectl -n thorium patch $TC --type merge \
     -p '{"spec":{"registry":"<registry>/thorium","version":"<tag>"}}'
   kubectl -n thorium rollout status deployment/operator --timeout=600s
   kubectl -n thorium get deploy -o wide
   ```

2. Download the stage-1 thoradm from the API (`<THORIUM_URL>/api/binaries/linux/x86-64/thoradm`).
3. Take and verify a backup with writers paused, as in
   [Taking a verified backup](#taking-a-verified-backup).

**Check.** Every component runs the stage-1 image, the UI and `thorctl` work, and both verify runs
exit 0. There is no schema record before the conversion, so there is no drift to read yet (a
thoradm that has `schema diff` exits 1 with "there is no recorded revision to compare with").

**Roll back.** Run step 1 again with the recorded stage-0 image (by digest when the tag moved:
`"version":"<tag>@sha256:<digest>"` in the patch and
`operator=<registry>/thorium:<tag>@sha256:<digest>`).

### Stage 2: Convert to Helm and apply the schema revisions

**What changes.** `convert-to-helm.sh` hands the deployment to the Helm charts: it stops the
pre-Helm operator, saves the `ThoriumCluster` in the `thorium-legacy-cluster` Secret, applies the
chart's CRD, records revision `2026-10-v01`, hands over or deletes the objects the chart replaces,
and deletes the `ThoriumCluster` without cleanup so the chart recreates it. The components keep
running, unmanaged, until the chart is deployed. With a target of `2026-10-v03`, the operator then
runs `2026-10-v02` (backend checks and an Elasticsearch reindex where the mappings are wrong) and
`2026-10-v03` (the schema apply and the user theme move). See the [change log](#revision-change-log).
The schema apply also recreates the materialized views an older release created with outdated
definitions and waits for them to build (see [Schema apply and drift](#schema-apply-and-drift)).

**Before you start** (the conversion's own list is in
[Before you start](./convert-to-helm.md#before-you-start)):

- Stage 1 is done.
- **A verified backup taken right before the conversion**, with writers paused until the
  conversion starts. It is the rollback point to stage 1, and the reindex approval names it. When
  stage 2 follows stage 1 at once with writers still paused, the stage-1 backup is this backup.
- `kubectl`, `helm` 3.8 or newer (Helm 4 works; see the known issues), `jq` 1.6 or newer, and the
  stage-2 release's `deploy/charts/scripts/convert-to-helm.sh`.
- **Proxy settings in each component's `env`.** The Thorium image carries no proxy variables. A
  component that needs a proxy (the scaler pulling from an outside registry, for example) sets
  `http_proxy`, `https_proxy`, and `no_proxy` in its `env`, and `no_proxy` covers `.svc`,
  `.cluster.local`, the cluster's service and pod CIDRs, and internal registries. The operator's
  thoradm Jobs take the API component's proxy settings, so give the API a `no_proxy` that covers
  every backend whenever it sets a proxy (see [Proxies](./helm-configuration.md#proxies)).
- **NetworkPolicies on the backing services admit the whole Thorium namespace**, not only the API's
  pods: the operator's schema and migration Jobs don't carry `app: api` (see the example in
  [Before you start](./convert-to-helm.md#before-you-start)).
- One Thorium deployment per Kubernetes cluster: retire other pre-Helm instances first
  ([Retiring other pre-Helm instances](./convert-to-helm.md#retiring-other-pre-helm-instances)).
- An existing admin's login for the chart's `thorium-admin` Secret.
- `helm registry login <registry>` on the host that runs the conversion and the install, when the
  charts come from a private OCI registry.
- `spec.registry` of the `ThoriumCluster` names the repository that holds the stage-2 image: the
  conversion carries it into `operator.image.repository`, while the tag comes from the chart.

**Steps.**

1. Preview with the chart you will install, passing every values file the deployment will also use:

   ```bash
   export THORIUM_ADMIN_PASSWORD='<existing admin password>'
   deploy/charts/scripts/convert-to-helm.sh --chart $CHART --chart-version $VERSION \
     --admin-user <admin> --values site.yaml --dry-run
   ```

   Review `thorium-converted-values.yaml` and the listed changes and collisions.

2. Convert: the same command without `--dry-run` (`--yes` skips the confirmation).
3. Deploy the chart with the converted values last and a target:

   ```bash
   helm upgrade --install thorium $CHART --version $VERSION -n thorium --skip-crds \
     -f site.yaml -f thorium-converted-values.yaml \
     --set operator.cluster.upgrade.targetRevision=2026-10-v03
   ```

   Keep the target in a values file afterwards so later upgrades keep it. On megathor, set
   `thorium_upgrade_target_revision` and put the converted values last in `thorium_values_files`.

   The schema-apply Job doesn't pause the API. When its log (or a `thoradm schema apply
   --dry-run`) names `committed_repo_data` among the views it recreates, don't ingest or update
   repos until the apply finishes, since an ingest can then delete repo data commits still use.

4. If `elastic-reindex-keyword-mappings` waits for an approval, approve it naming the backup, and
   run `helm upgrade` again:

   ```yaml
   operator:
     cluster:
       upgrade:
         targetRevision: 2026-10-v03
         approvals:
           - step: elastic-reindex-keyword-mappings
             backup: thoradm backup <date> on <where>
   ```

5. Once the phase is `Ready`, run `deploy/charts/scripts/convert-to-helm.sh --cleanup`. Keep the
   `thorium-legacy-cluster` Secret until you no longer want to roll back.

Never recover from a failed install with `helm uninstall` (without `--no-hooks`) or
`helm install --atomic`: the uninstall hook deletes the `ThoriumCluster`. Fix the cause and run
`helm upgrade --install` again.

**Check:**

```bash
kubectl -n thorium get thoriumcluster -o wide          # phase Ready, REVISION 2026-10-v03
kubectl -n thorium get configmap thorium-upgrade-state -o jsonpath='{.data.state\.json}' | jq .
thoradm --cluster-conf thorium.yml schema status       # the record in Scylla
thoradm --cluster-conf thorium.yml schema diff         # exit 0, or 7 with only expected drift
```

The upgrade history in the `thorium-upgrade-state` ConfigMap (the `applied` entries in
`state.json`, shown by the command above) lists each step `Done` or `Skipped`.
`status.upgrade.steps` is empty once the phase is `Ready`, and `status.upgrade.conditions` holds
no `CatalogSkew` or `SchemaSkew`. A long-lived keyspace commonly shows drift that is expected at
this stage; [Reading schema drift](#reading-schema-drift) lists it and tells it from drift that
needs a look. Run `schema diff` at this check and keep its output as this stage's snapshot. The
`SchemaDrift` condition shows the operator's last drift read, which can be up to a day old on a
`Ready` cluster. Restarting the operator (`kubectl -n thorium rollout restart deployment operator`)
triggers a reconcile, which reads the drift again only when a read is due: the last read (kept in
the `thorium-upgrade-state` ConfigMap) is over an hour old, the acknowledgements changed, or a
schema apply finished since it.
(`convert-to-helm.sh` records `2026-10-v01` only in the `thorium-upgrade-state` ConfigMap; until
the chart's operator copies it into Scylla, `schema diff` exits 1 with "there is no recorded
revision to compare with".) To keep a before-apply snapshot too, run it once
`thoradm schema status` shows the recorded revision and before the schema-apply step runs. Log in
to the UI and `thorctl`, list files, images, and pipelines, run a reaction, then resume the writers.

**Roll back:**

- Before the chart is deployed: run the old minithor or megathor again (or reapply the stage-1
  operator Deployment), recreate the `ThoriumCluster` from the `thorium-legacy-cluster` Secret, and
  remove the conversion's leftovers ([Rolling back](./convert-to-helm.md#rolling-back)).
- After the chart is deployed but before a target is set: nothing in the databases changed; fix
  the problem and run `helm upgrade --install` again.
- After steps ran: the Helm release owns the deployment, so going back to stage 1 means restoring
  the pre-conversion backup into a fresh stage-1 install (see [Rolling back a stage](#rolling-back-a-stage)).

### Stage 3: Upgrade to the follow-up release

**What changes.** One `helm upgrade` to the later chart. With a target of `2026-10-v05`, the
operator runs `2026-10-v04` (it waits for in-flight reactions to finish, converts every image to a
tool, pins pipeline stages to those tools, and moves legacy tool names in provenance) and
`2026-10-v05` (records the YARA scanning schema), then applies the image's declared schema, which
creates the `scan_stats` table and the `entities_by_name` view, before it rolls out the new
components. No schema apply runs before the conversions on this path (see the `scan_stats` row in
[Upgrade control](#upgrade-control)). The three
conversion steps can't be undone and pause the API, the scalers, the event handler, and the search
streamer until the new Deployments are applied. [Upgrading to Agentic
Workflows](../agentic/upgrading.md) covers them in full.

**Before you start:**

- Stage 2 is done: phase `Ready` at `2026-10-v03`. That holds because `2026-10-v03` is the stage-2
  operator's latest revision; a newer operator held at a target of `2026-10-v03` shows
  `UpgradeRequired` instead.
- The outage is planned and users are told. What creates reactions (triggers, bulk submissions,
  users) is stopped, and running reactions are left to finish: the drain never ends while something
  keeps submitting.
- Images whose `env` holds secrets are cleaned up while image writes still work: after the upgrade,
  image writes return `410 Gone`, and tool instance variables are visible to every group that can
  read the tool.
- A verified backup taken right before the upgrade, with writers paused until it starts. Every
  approval names it, and it is the rollback point to stage 2.
- Components that need a proxy keep their proxy `env` (as in stage 2).
- **No values file pins the stage-2 image.** The chart's pre-upgrade hook applies the
  `ThoriumCluster` CRD from the image the values name (`operator.image.tag`, the chart's
  `appVersion` when unset), not from the chart. Remove an `operator.image.tag` override left
  from stage 2, or set it to the stage-3 image's tag, in every values file you pass; otherwise the
  hook puts the stage-2 CRD back and the upgrade fails (see the known issues). Check with
  `helm get values thorium -n thorium --all`.
- Repo ingests stay held while the schema apply recreates `committed_repo_data`, as in stage 2.
- **The YARA ruleset cache bucket.** Stage 3 adds one bucket, `thorium.yara.cache_bucket`
  (`<namespace>-yara-rulesets` from the chart, so `thorium-yara-rulesets` in the `thorium`
  namespace). To name it like the deployment's other buckets, set
  `operator.cluster.config.thorium.yara.cache_bucket` in the values file passed last. The operator
  creates it when HeadBucket answers 404 and skips a bucket that exists, so pre-create it where
  Thorium's S3 identity can't create buckets or the store refuses the operator's request (see
  [External services](./helm-configuration.md#external-services)).

**Steps.**

1. List the steps with the stage-3 thoradm (no cluster config needed):

   ```bash
   thoradm migrate plan --from 2026-10-v03 --to 2026-10-v05
   ```

2. Upgrade with the target and every approval at once, so no step waits for another `helm upgrade`:

   ```yaml
   # upgrade.yaml
   operator:
     cluster:
       upgrade:
         targetRevision: 2026-10-v05
         approvals:
           - step: images-to-tools
             backup: thoradm backup <date> on <where>
           - step: pipelines-to-tool-stages
             backup: thoradm backup <date> on <where>
           - step: provenance-to-legacy-tools
             backup: thoradm backup <date> on <where>
   ```

   ```bash
   helm upgrade thorium $CHART --version $VERSION -n thorium --skip-crds \
     -f site.yaml -f thorium-converted-values.yaml -f upgrade.yaml
   ```

   `--skip-crds` keeps Helm 4 from applying the chart's `crds/` if the command ever installs (as in
   stage 2); the pre-upgrade hook applies the CRD on an upgrade.

3. Watch the drain and the steps; while the upgrade waits at `drain-reactions`, Thorium still
   serves:

   ```bash
   kubectl -n thorium describe thoriumcluster
   thoradm --cluster-conf thorium.yml migrate check --id drain-reactions
   thoradm --cluster-conf thorium.yml migrate check --id images-to-tools --all --samples
   ```

   Delete stuck reactions while the previous API still runs, and don't restart API pods meanwhile.
   When the wait doesn't end although nothing seems to run, see [A drain that doesn't
   end](#a-drain-that-doesnt-end).

**Check.** Phase `Ready` at `2026-10-v05` and every component back at its replica count;
`thoradm --cluster-conf thorium.yml schema diff` exits 0, or 7 with only the drift
[expected after stage 3](#expected-drift) (keep its output as this stage's snapshot);
`thoradm --cluster-conf thorium.yml migrate report --id images-to-tools` (and the two other
conversions) shows what changed and what was skipped; tell group owners about skipped images.
`SchemaDrift` shows the operator's last drift read, which may predate this stage (see stage 2);
`thoradm schema diff` shows the live state.
Migrated tools show in `thorctl tools get` and the UI, pipelines name a tool for every stage, and a
reaction runs to completion. Remove the approvals `status.upgrade.stale_approvals` lists from your
values.

**Roll back.** A target below `2026-10-v04` is refused once a conversion step finished or was
skipped. Restore the pre-upgrade backup into a fresh stage-2 install (see [Rolling back a
stage](#rolling-back-a-stage)).

### Taking a verified backup

Every stage starts with this. A backup is read table by table, not as a snapshot, so pause writers
first:

1. Tell users. Scale the `operator` Deployment to zero first (its reconcile puts the component
   Deployments back, replicas included), then the scaler, event handler, search streamer, and
   reactors. Keep the API up: the Redis data is read through it.
2. Back up every component into a new directory, with the thoradm that matches the API:

   ```bash
   thoradm --cluster-conf thorium.yml backup new --output /backups/thorium-<stage>-<date>
   ```

   Exit 0 means complete; exit 2 means it finished with records in `failures.jsonl` or
   `pruned.jsonl` (read [Exit codes](../thoradm/taking_backups.md#exit-codes)); exit 1 means it
   stopped early, and the same command resumes it.
3. Verify it offline, then against the cluster:

   ```bash
   thoradm backup verify --backup /backups/thorium-<stage>-<date>
   thoradm --cluster-conf thorium.yml backup verify --cluster --backup /backups/thorium-<stage>-<date>
   ```

   With writers still paused, the cluster check should find no differences.
4. Keep the backup with the cluster config, the Thorctl config, your values files, and the
   deployment's `thorium.secret_key`: a restore into another deployment needs the source's
   `secret_key`, or no restored password verifies.
5. Resume the writers (scale the operator back to 1; its first reconcile restores the components),
   or keep them paused when the next stage follows at once.

With the thoradm from the latest release (stage 3), a backup location can be shared with an admin
group by creating the directory with the setgid bit (`install -d -m 2770 -g <admins> DIR`); see
[Disk space and permissions](../thoradm/taking_backups.md#disk-space-and-permissions). The stage-1
and stage-2 thoradm keep the backup private to the user who ran it whatever the directory's mode, so
use a directory only that user can read and run every step of the backup as that user.

A Redis record the API can't parse doesn't fail the backup: the API leaves it out, with the records
that depend on it, and thoradm lists each in `failures.jsonl` and exits 2 (see the [known
issues](#backups-and-restores)). This holds on all three stages; on the stage-1 and stage-2 builds
the dependents are the images and pipelines of a skipped group and the pipelines that run a skipped
image.

### Rolling back a stage

Rolling back installs the earlier stage fresh and restores that stage's backup into it with that
install's own thoradm; never restore with a newer thoradm and then downgrade the images.

| Return to | Backup | Restored with |
|-----------|--------|---------------|
| Stage 1 (pre-Helm) | the backup taken right before the conversion | the stage-1 thoradm, into a fresh stage-1 install |
| Stage 2 (Helm, `2026-10-v03`) | the backup taken right before stage 3 (or, losing what was written since, the pre-conversion backup) | the stage-2 thoradm, into a fresh stage-2 install |

For every restore:

- Give the fresh install the source's `thorium.namespace` and `thorium.secret_key` before
  restoring.
- Install a fresh chart with `operator.toolbox.enabled=false`: the toolbox import writes rows that
  make a plain restore refuse a non-empty target. On a stage-2 install, also set the target
  `2026-10-v03`.
- To reuse the same Kubernetes cluster and backing services, empty them first: remove Thorium
  (`helm uninstall`, or delete the pre-Helm `ThoriumCluster` and operator), delete the
  `thorium-upgrade-state` ConfigMap, the `thorium-operator-pass` Secret, the group namespaces, and
  the conversion's leftovers, and drop the Scylla keyspace named by `thorium.namespace` (then
  `nodetool clearsnapshot <keyspace>` on each node). Redis needs nothing (a restore deletes the
  namespace's keys first), S3 needs nothing, and search is rebuilt after the restore.
- Pause writers, restore, verify, then rebuild search (the search streamer's `--reindex`, see
  [Reindexing Elasticsearch](./upgrades.md#reindexing-elasticsearch)) and resume the writers:

  ```bash
  thoradm --cluster-conf thorium.yml backup restore --backup <dir> --confirm
  thoradm --cluster-conf thorium.yml backup verify --cluster --backup <dir>
  ```

A backup without a schema record (stage 1) restored into a deployment that records a newer
revision is recorded at `2026-10-v02`, and the operator runs every later step on the restored
data (see [The Schema Record](../thoradm/thoradm.md#the-schema-record)). Reactions, jobs, workers,
stage logs, and events aren't backed up, and anything written after the backup is lost.

## From a recorded revision

A Helm deployment at a recorded revision upgrades with one `helm upgrade` to the new chart and a
target of the new release's latest revision:

1. Take and verify a backup right before setting the target ([Taking a verified
   backup](#taking-a-verified-backup)).
2. List the steps: `thoradm migrate plan --from <current revision>`.
3. Check for drift: `thoradm --cluster-conf thorium.yml schema diff`, and compare what it lists
   with [Expected drift](#expected-drift). Drift blocks only the steps whose targets overlap it.
4. Deploy the new chart with your values, the target, and an approval naming the backup for every
   `Data` step that has work to do.
5. Check the phase, revision, steps, and conditions as in stage 2, and run `schema diff` again.

A release upgrades a deployment at any revision from its oldest supported one on; older deployments install
the intermediate releases first (the [upgrade paths table](./upgrades.md#upgrade-paths)).

## Reading schema drift

Drift is the live schema differing from the snapshot of the revision the deployment records. A
keyspace that older releases created carries objects later code doesn't declare, so some drift is
normal, and which drift depends on how old the deployment is and which stage it is at. This
section shows how to read `thoradm schema diff`, lists the drift each stage can expect, and says
what to do with anything else. How the operator reads drift and how acknowledgements work is in
[Schema drift](./upgrades.md#schema-drift).

### The diff report

Run the thoradm that matches the API, from a host with the cluster config or in the API pod:

```bash
thoradm --cluster-conf thorium.yml schema diff
kubectl -n thorium exec deploy/api -- /app/thoradm --cluster-conf /conf/thorium.yml schema diff
```

The cluster config option is `--cluster-conf` (short `-c`), given before `schema`; thoradm has no
`--config` option. `--json` prints the whole report as JSON. The command only reads: it probes
Scylla, the configured Elasticsearch indexes, and the Redis and S3 layouts the deployment records.
The report goes to standard output; on drift, thoradm also logs `Schema drift: the live schema
differs from revision <revision> (the report is above)`. Here is part of the report from a keyspace
that releases before the first revision created (Scylla 5.2), recorded at `2026-10-v03` before its
schema apply recreated the stale views. `…` marks left-out objects, and fingerprints are shortened;
they differ between deployments:

```text
revision 2026-10-v03 (expected deployment fingerprint 86722e96…, observed 34449a8c…)
scylla   drift
  unexpected  associations_by_kind
      fingerprint a260ac97…
  …
  unexpected  commits (acknowledged)
      fingerprint 787c6ddd…
  …
  changed     committed_repo_data
      fingerprint 40c3843f…
      base: expected commitish_list, observed commits_list
  …
  changed     committed_repo_data.key
      key: expected text, clustering key 3 asc, observed absent
  …
  changed     results_ids
      fingerprint 3d7de794…
      filter: expected absent, observed tool is not null
  …
elastic  ok
redis    ok
s3       ok
```

- **The revision line** names the revision whose snapshot the live schema is compared with: the
  recorded one, or, while an upgrade's schema apply has run ahead of the record, the revision that
  apply converged to. The expected deployment fingerprint is that snapshot's; the observed one is a
  hash of everything the probe saw, extras included. They differ whenever any object differs,
  acknowledged ones too, so read the backend lines, not the fingerprints, for the verdict.
- **Each backend line** is `ok` (nothing differs, or only acknowledged objects do) or `drift` (at
  least one unacknowledged object). Scylla and the configured Elasticsearch indexes are always
  listed; Redis and S3 are listed once a schema apply has recorded their layouts, so a deployment
  whose record was only seeded (the conversion's `2026-10-v01`) shows neither.
- **Each object** is listed under its backend with its kind: `unexpected` (live, but not in the
  snapshot), `changed` (in both, but different), or `missing` (in the snapshot, but not live). A
  difference in one column of a table or view, or one field of an index, is named
  `<object>.<column>` (`committed_repo_data.key`, `SampleTags.group`); a difference in a view's base
  table or filter is named after the view. Each `changed` entry has one `<part>: expected …,
  observed …` line per part (`absent` when a side has none).
- **`(acknowledged)`** marks an object that raises no drift: a retired object Thorium registers as
  legacy, an object a later revision declares (see the table below), or one in the
  `--acknowledged <file>` list. `(its acknowledgement lapsed: the object changed)` marks an
  acknowledgement whose fingerprint no longer matches.
- **`fingerprint`** is the hash of the live object (or column) that an acknowledgement can name. An
  entry for a column that isn't live (`observed absent`) has no fingerprint line.

`schema diff` doesn't read the `ThoriumCluster`, so the acknowledgements in your values aren't
applied unless you pass them. To see what the operator sees, save them and pass the file:

```bash
ACKS=$(kubectl -n thorium get thoriumcluster -o jsonpath='{.items[0].spec.upgrade.drift_acknowledged}')
echo "${ACKS:-[]}" > acks.json
thoradm --cluster-conf thorium.yml schema diff --acknowledged acks.json
```

Exit codes:

| Code | Meaning |
|------|---------|
| 0 | Every backend is `ok` |
| 7 | At least one unacknowledged object differs; the full report is still printed |
| 1 | No recorded revision to compare with (before the conversion's record reaches Scylla, or on a keyspace no schema apply has recorded): `there is no recorded revision to compare with` |
| 2 | The cluster config or the `--acknowledged` file can't be used |
| 3 | Scylla can't be reached |
| 4 | A read failed in a way a retry may get past |

The operator runs the same comparison in a `thoradm schema status` Job, with the spec's
acknowledgements applied, and shows the result in `status.upgrade.schema`, `status.upgrade.drift`,
and the `SchemaDrift` condition (for example `scylla: 11 changed, 13 unexpected not acknowledged
(thoradm schema diff for details)`). Which drift blocks which upgrade step, and how to acknowledge
it, is in [Schema drift](./upgrades.md#schema-drift).

### Expected drift

These are what `schema diff` printed on keyspaces that releases before the first revision created,
taken through the stages above. The revision is the one on the report's first line.

| What `schema diff` shows | When it shows | What to do |
|--------------------------|---------------|------------|
| `unexpected` `commits`, `commits_list`, `cursors`, `simple_cursors`, `workers`, `exports`, `export_errors`, and `export_error_ids`, each `(acknowledged)` | At every revision, on a keyspace that has them (`thoradm schema legacy` lists them as created by a release before the first revision). No schema apply or upgrade step removes them | Nothing. To reclaim the space, drop them on purpose with `thoradm schema drop-legacy` (read [Legacy Schema Objects](../thoradm/thoradm.md#legacy-schema-objects) first) |
| `unexpected  events (acknowledged)` | From `2026-10-v03` on, on a keyspace that has the table. It isn't listed at `2026-10-v01`, whose snapshot includes it (`schema legacy`: created by `2026-10-v01`, `2026-10-v02`) | As above |
| `unexpected  entities_by_name (acknowledged)` | At `2026-10-v03`, on a keyspace that has the view. It isn't listed at `2026-10-v01`, whose snapshot includes it, nor once the stage-3 schema apply has run, since `2026-10-v05` declares it again | Nothing; `2026-10-v05` declares it for exact-name entity reads |
| `unexpected` `schema_migrations` and `schema_migration_issues`, without `(acknowledged)` | At `2026-10-v01`, only with builds that still compare the upgrade system's own tables (early stage-2 images) | Use a build that leaves them out of the comparison. Never drop these tables: they hold the schema record |
| `changed     committed_repo_data` with `base: expected commitish_list, observed commits_list`, and `changed` entries for its columns `group`, `hash`, `key`, `kind`, `repo`, and `timestamp` (such as `key: expected text, clustering key 3 asc, observed absent`) | Before the stage-2 schema apply, on a keyspace whose view an older release built on `commits_list` | Hold repo ingests until the schema apply has recreated the view (`thoradm schema apply --dry-run` lists the views it would recreate); the entries are gone after it. See [Stale materialized views](#stale-materialized-views) |
| `changed` `repos`, `results_auth`, `results_ids`, and `samples`, each with a `filter: expected absent, observed … is not null …` line naming non-key columns | Before the stage-2 schema apply, on a keyspace whose views were created with those filters (Scylla 5 accepted them) | As above: the schema apply recreates them, and the entries are gone after it |
| `changed` Elasticsearch fields, such as `SampleTags.group` with `group: expected {"type":"keyword"}, observed {"type":"text"}` (the backend reads `drift`) | Indexes made without Thorium's mappings, until stage 2's `elastic-reindex-keyword-mappings` step rebuilds them. Right after the rebuild, the index was listed as `missing` | Approve that step |
| `missing` tables or views, such as `missing     nodes` | A keyspace that lacks objects the recorded revision lists | The next schema apply creates them, and the entries are gone after it |

Tables and views that feature or development builds created aren't registered, so they show as
`unexpected` without `(acknowledged)`: the backend reads `drift`, `schema diff` exits 7, and the
operator sets the `SchemaDrift` condition. On one long-lived dev keyspace these were the view
`associations_by_kind` and the tables `commitish`, `tools`, and `tools_list`; they stayed listed
after the stage-2 and stage-3 schema applies, and `thoradm schema legacy` counts them as unmanaged
extra objects without naming them. Investigate before dropping anything: find out which build created them, and look at
their rows (`SELECT * FROM <namespace>.<table> LIMIT 10` in `cqlsh`). Then either acknowledge each
in `driftAcknowledged` with the fingerprint `schema diff --json` prints (see the command under
[Schema apply and drift](#schema-apply-and-drift)), or drop it by hand. A thoradm backup reads only
the tables Thorium knows, so take a Scylla snapshot (`nodetool snapshot`) of the table first if its
rows might matter.

Anything else is unexpected drift: a `changed` table or column, a `missing` key column, an
`unexpected` index or column that no row above explains. Before going on:

1. Take and verify a backup ([Taking a verified backup](#taking-a-verified-backup)), and don't set
   a new target.
2. Compare the report with the snapshot you kept at the end of the previous stage: drift that
   appeared since then came from something that ran in between (a hand-made change, a feature
   build, a restore from another deployment).
3. Report it with the `schema diff --json` output, the `schema status` output, and the image
   digests and revision the deployment runs. Restore a `changed` object to its recorded definition,
   or acknowledge an `unexpected` one only once you know where it came from.

### Stale materialized views

Scylla can't alter a materialized view, so a view an older release created keeps its definition
until a schema apply recreates it. The schema apply (the schema-apply Job in an upgrade, or
`thoradm schema apply`) compares each declared view with the release's definition, logs how each
differs, drops and recreates each one that differs, and waits for every node to build it.
`thoradm --cluster-conf thorium.yml schema apply --dry-run` lists the views a run would recreate,
and why, without changing anything.

A rebuild scans the view's base table. In a dev upgrade of a long-lived keyspace, about 1.1 million
view rows across five views rebuilt in about 7 seconds on a three-node cluster; benchmarks measured
10 to 25 seconds per million rows per view, and rates vary with load. Size the base tables first
(`nodetool tablestats <namespace>.<table>` on a Scylla node). A build that outlasts the view wait
(`--view-wait-secs`, five hours by default) fails the apply naming the views and nodes still
building; run `thoradm --cluster-conf thorium.yml schema apply --view-wait-secs <secs>` yourself
with a longer wait, then set the `retry-schema-apply` annotation (see
[Schema apply and drift](#schema-apply-and-drift)). While a view rebuilds, routes that read it
fail while it is dropped and return incomplete listings until its build finishes, and the
schema-apply Job doesn't pause the API: hold repo ingests while `committed_repo_data` is recreated
(see [Schema apply and drift](#schema-apply-and-drift)).

Check the rebuild afterwards:

- `thoradm --cluster-conf thorium.yml schema diff` no longer lists the view.
- `thoradm --cluster-conf thorium.yml schema verify-views --view <view>` compares the view with its
  base table by token range (exit 0 when they agree).
- Row counts in `cqlsh`, for example `SELECT COUNT(*) FROM <namespace>.committed_repo_data;` and
  the same on `commitish_list`. On a large table `COUNT(*)` needs a longer client timeout (`cqlsh
  --request-timeout 3600`). A view skips base rows that leave one of its key columns empty, so its
  count can be lower than the base table's, and a plain `SELECT` can't filter the base table the
  same way, since `IS NOT NULL` is only accepted in a view's definition.

## Revision change log

Each revision names a point in Thorium's deployment history. A release knows every revision up to
its own latest, and the operator runs the steps between the deployment's recorded revision and the
target. `thoradm migrate list` prints the catalog of the build you run; [Revisions](./upgrades.md#revisions)
has each step in full.

| Revision | Reached from | What changes for admins | Steps (kind) | Approval and outage | Downgrade |
|----------|--------------|-------------------------|--------------|---------------------|-----------|
| `2026-10-v01` | — | The baseline: a pre-Helm deployment once `convert-to-helm.sh` has recorded it | none | none | — |
| `2026-10-v02` | `2026-10-v01` | Thorium deployed by the Helm charts with the chart-managed operator. Thorium's Scylla role and Elasticsearch user are checked (and created when the bootstrap is set); Elasticsearch indexes that are missing or mapped differently (megathor-made indexes whose `group` field is `text`) are rebuilt by the search streamer | `scylla-role` (Infra), `elastic-identity` (Infra), `elastic-reindex-keyword-mappings` (Data), `finalize` (Config) | The reindex needs an approval only when an index is wrong; it pauses only the search streamer, and search is incomplete until the reingest finishes (`SearchIncomplete`) | One-way |
| `2026-10-v03` | `2026-10-v02` | The declared schema is applied in a Job before any step that changes data, and the record of it is kept in Scylla's `schema_migrations`; each user's theme moves into the user's settings | `schema-apply` (Infra), `users-settings-theme` (Data) | The theme move needs an approval only when a user still holds the theme in its own field; it pauses the API | To `2026-10-v02` with `users-settings-theme-reverse`, which loses nothing |
| `2026-10-v04` | `2026-10-v03` | Images become tools with a first tool instance, pipeline stages are pinned to those tools, and legacy tool names in provenance move to their own field; image writes return `410 Gone` afterwards | `drain-reactions` (Data, waits only), `images-to-tools`, `pipelines-to-tool-stages`, `provenance-to-legacy-tools` (Data), `finalize-tool-only` (Config) | Each conversion needs an approval when it has work; they pause the API, scalers, event handler, and search streamer until the new Deployments are applied | Refused once a conversion step finished or was skipped: restore a backup |
| `2026-10-v05` | `2026-10-v04` | The YARA scanning schema: the `scan_stats` table, the `entities_by_name` view, and the `yara_rulesets` bucket, created by the schema apply and the reconcile | `finalize-yara-scanning` (Config) | none | To `2026-10-v04` with `finalize-yara-scanning-reverse`, which drops nothing |

Revisions from `2026-10-v04` on hold no schema step of their own: the operator applies the schema of
the image it deploys before it rolls anything out, whenever the image, the revision, or the
data-shaping settings change.

## Known issues and remediation

These come from converting and upgrading long-lived production deployments. Each row says what you
see, why, and what to do.

### Conversion and Helm

| Symptom | Cause | Fix |
|---------|-------|-----|
| `convert-to-helm.sh` stops with `syntax error, unexpected label` | An older script on a host with jq 1.6 | Use the current release's script, which runs on jq 1.6 and newer |
| The first `helm upgrade --install` after the conversion fails with `failed to install CRD crds/thoriumcluster.yaml: conflict occurred ... conflicts with "thorium_cluster_apply"` | Helm 4 applies the chart's `crds/` server-side, and the conversion (or the operator) already applied the CRD | Pass `--skip-crds` on the install, as the conversion prints; the chart's pre-upgrade hook and the operator keep the CRD current |
| Helm 4 fails with `conflict with "kubectl-create"` (or `"before-first-apply"`) on the `banner` ConfigMap, the `thorium` ServiceAccount, or a namespace | A deployment converted by an older `convert-to-helm.sh`, which left the adopted objects' fields with the scripts' field manager | For the `banner` ConfigMap, delete it and run the install again, so the chart creates it (check that `operator.banner` in the converted values holds the banner you want). For other objects, hand their fields to the release with the patch in [Helm install conflicts](./troubleshooting.md#helm-install-conflicts). Setting `managedFields` to `[{}]` doesn't help |
| The login banner loses its trailing newline, or Helm wants to change the banner after a conversion | An older conversion dropped the banner's trailing newline | Set `operator.banner` in the converted values to the exact text; the current script carries it over byte for byte |
| `helm upgrade` to a newer chart fails with `server-side apply failed for object thorium/<name> sandia.gov/v1, Kind=ThoriumCluster ... field not declared in schema` (for example `.spec.config.thorium.yara`), and applying the new CRD by hand doesn't help | A values file still sets `operator.image.tag` to the previous release's image. The chart's pre-upgrade hook runs `thorium-operator crd --apply` from the image the values name, so it applies the previous release's CRD over the new one with every attempt, and the old operator does the same when it starts | Remove the override or set it to the new image's tag in every values file, check with `helm get values thorium -n thorium --all`, and run `helm upgrade` again. Move or remove an image tag override with every chart upgrade |
| A later `helm upgrade` fails with a conflict naming `kubectl-edit` or `kubectl-patch` on the `ThoriumCluster` | Someone changed the Helm-owned `ThoriumCluster` with `kubectl edit` or `kubectl patch`; that manager now owns the fields, and Helm 4 refuses to change them (Helm 3 doesn't check field ownership, so the edit and the values can silently disagree) | Make every change through the values files. To clear the conflict, put the change in your values, then remove the editing manager's entry from the object's `managedFields` (below) and run `helm upgrade` again. Annotations the chart doesn't set, such as `thorium.sandia.gov/retry-schema-apply`, are safe to set with `kubectl annotate` |
| A setting from `site.yaml`, such as a component's proxy `env`, is missing after the install | Helm merges maps but replaces lists: the converted values are passed last, so a list there (a component's `env` or `args`, for example) replaces the whole list from `site.yaml` | Put the complete list in the file passed last (edit `thorium-converted-values.yaml`, or pass a final values file that holds the full list), and check the result with `helm get values thorium -n thorium --all` |
| A setting changed in `site.yaml` (or a `--set` before the converted values) has no effect after the conversion, such as `clear_proxy` staying `false` | The converted values file is passed after `site.yaml`, so its values win, and it carries every setting the pre-Helm operator stored in the `ThoriumCluster`, defaults included | Change such settings in `thorium-converted-values.yaml` (or remove them there so `site.yaml` decides), and check the result with `helm get values thorium -n thorium --all` |
| External MCP clients lose their sessions, or a hand-made `/api/mcp` route still exists after the upgrade | The MCP server keeps sessions in the one API pod the `thorium-mcp` Service selects; the conversion deletes the legacy ingress with any route added to it, and the chart's ingress now sends `/api/mcp` to `thorium-mcp` itself | Delete any separate IngressRoute or Ingress made by hand for `/api/mcp` after upgrading; see [Ingress](./helm-configuration.md#ingress) |

Remove the manager that edited the `ThoriumCluster` (here `kubectl-edit`) from its `managedFields`:

```bash
TC=$(kubectl -n thorium get thoriumcluster -o name)
i=$(kubectl -n thorium get $TC -o json \
  | jq '[.metadata.managedFields[].manager] | index("kubectl-edit")')
kubectl -n thorium patch $TC --type json \
  -p "[{\"op\":\"remove\",\"path\":\"/metadata/managedFields/$i\"}]"
```

### Tool bans

| Symptom | Cause | Fix |
|---------|-------|-----|
| A tool instance converted from a banned image (or a later version of its tool) runs, or `thoradm bans audit` lists an image with `"covered": false` | The deployment ran `2026-10-v04` before conversions carried image bans, so the converted instances and the versions published from them have none | Upgrade the API to a release with the fix: the consistency scan it runs at startup adds each legacy image's bans to every instance of its converted tool. Run `thoradm bans audit` to confirm every converted image shows `"covered": true` (see [Tool Instance Bans](../bans_admins.md#tool-instance-bans)) |

### Buckets

| Symptom | Cause | Fix |
|---------|-------|-----|
| The phase is `Error` with `Failed to create bucket <bucket> at <endpoint> (thorium.s3.region: <region>): ...`, or a bare `InvalidArgument` from S3 | The store refused the CreateBucket request. An operator built without the location-constraint check sends an empty `CreateBucketConfiguration` when `thorium.s3.region` is unset, empty, or `us-east-1`, which Ceph RGW refuses with `InvalidArgument` | Pre-create the bucket the message names (`<namespace>-yara-rulesets` on stage 3, unless `thorium.yara.cache_bucket` names another): the operator skips a bucket that exists. A stage-1 operator doesn't check whether a bucket exists, so on stage 1 pre-create every bucket and set `thorium.s3.skip_bucket_auto_create: true` in the `ThoriumCluster`'s `spec.config` |

### Proxies and network policies

| Symptom | Cause | Fix |
|---------|-------|-----|
| The schema-status or a migration Job fails with `error sending request for url (...)` while the API reaches the same backend | The Job's requests go to a proxy: an image built on a base that bakes in `http_proxy`/`https_proxy` without an in-cluster `no_proxy` sends in-cluster traffic to that proxy | Use an image that carries no proxy variables. Give each component that needs a proxy its own `env` (`http_proxy`, `https_proxy`, `no_proxy`); the operator sets each variable in both cases and gives its thoradm Jobs the API's proxy settings. `no_proxy` must cover `.svc`, `.cluster.local`, the cluster's service and pod CIDRs, and internal registries |
| The scaler schedules nothing, logging `No clusters are available to schedule on`, or its Kubernetes client fails with `TunnelUnsuccessful` | The scaler's Kubernetes client honours `HTTPS_PROXY` but ignores `NO_PROXY`, so for a scaler that uses the `kube-config` Secret (`service_account: false`; `clear_proxy` applies only to that kube config) a proxy in its `env` (needed to pull from an outside registry, for example) also carries its connection to the Kubernetes API, which the proxy refuses | Set `operator.cluster.config.thorium.scaler.k8s.clear_proxy: true` (in the converted values file after a conversion, see above), so the client connects to the Kubernetes API directly while the rest of the scaler keeps the proxy |
| The same error with `timed out` after the URL | A NetworkPolicy in the backend's namespace admits only `app: api` pods; the operator's Jobs carry their own labels | Admit the whole Thorium namespace on each backend's port ([example](./convert-to-helm.md#before-you-start)) |
| The error names `dns error`, `Connection refused`, or a certificate error | The cause is in the message: an unresolvable name, nothing listening, or TLS verification | See [The schema probe reports "error sending request"](./troubleshooting.md#the-schema-probe-reports-error-sending-request) |

### Schema apply and drift

| Symptom | Cause | Fix |
|---------|-------|-----|
| A rebuilt image pushed under the same tag changes nothing, or the schema apply doesn't run again (`MutableImageTag`) | With a mutable tag, `helm upgrade` leaves the pod specs unchanged, so nothing restarts and pods keep the image they pulled; and the operator reruns the schema apply only when its trigger key changes (image reference, revision, settings), which the same tag doesn't | Prefer immutable tags or versioned charts. Otherwise restart the operator to pull its rebuilt image (`kubectl -n thorium rollout restart deployment/operator`), restart the components (`kubectl -n thorium rollout restart deployment`), and set the annotation below to a new value to run the schema apply again |
| The schema-apply Job failed and the phase is `Error` | The Job's logs are in `status.upgrade.failure`; it isn't retried by itself | Fix the cause, then set the annotation below to a new value |
| `SchemaDrift` on a long-lived keyspace, listing the tables `commits`, `commits_list`, `cursors`, `simple_cursors`, `workers`, `exports`, `export_errors`, or `events`, or the view `export_error_ids` | Objects older releases created and later code stopped using: `commits` and `commits_list` were replaced by the commitish tables, `cursors`, `simple_cursors`, and `workers` moved to Redis, the exports objects were removed in 1.1.0, and `events` is no longer used | Builds that register them as legacy objects acknowledge them for you and report no drift; `thoradm schema drop-legacy` removes them on purpose ([Legacy Schema Objects](../thoradm/thoradm.md#legacy-schema-objects)). On a build that still reports them, acknowledge each in `operator.cluster.upgrade.driftAcknowledged` with its fingerprint (below). [Expected drift](#expected-drift) lists which deployments see them |
| `SchemaDrift` naming `schema_migrations` or `schema_migration_issues` as unexpected, on a deployment recorded at `2026-10-v01` or `2026-10-v02` | The upgrade system's own tables, created by every schema apply, `schema record`, and `migrate`, which only the snapshots from `2026-10-v03` on list | Builds that leave these tables out of the comparison never report them, whatever revision the deployment records. On a build that still reports them, acknowledge both (below) |
| `SchemaDrift` naming other unexpected tables, columns, or indexes | Leftovers from an unreleased or feature build that once ran against the keyspace | Check they hold nothing you need, then acknowledge each in `operator.cluster.upgrade.driftAcknowledged` with the fingerprint from `thoradm schema diff --json` (below), or drop them by hand. Drift blocks only the steps whose targets overlap a drifted object |
| Repos that were ingested show no data, or listings read through a view miss rows, on a keyspace created before mid-2024 or under Scylla 5 | A materialized view created by an old release with an outdated definition: `committed_repo_data` built on the retired `commits_list` table instead of `commitish_list`, or old `IS NOT NULL` filters that hide rows. Repo ingests then deleted `repo_data` rows the stale view still referenced | The schema apply drops and recreates each declared view whose live definition differs from the release's, logs how it differed (a `--dry-run` lists the views it would recreate), and waits for every node to build it before anything rolls out: about 10 to 25 seconds per million base-table rows per view. Routes reading a view fail while it is dropped and miss rows until its build finishes. To recreate one yourself, with writers paused: `thoradm --cluster-conf thorium.yml schema apply --recreate-view <view>`; `thoradm schema verify-views` compares a view with its base table. Until the view is rebuilt, don't ingest or update repos (`thorctl repos ingest`, commitish uploads, and any trigger or schedule that does either), since each ingest can delete more rows. The rebuild is done when `SELECT * FROM system.built_views WHERE keyspace_name = '<namespace>'` lists the view and `system.scylla_views_builds_in_progress` holds no row for it (check with `cqlsh`). Rows already lost come back by ingesting the affected repos again (`thorctl repos ingest`) once it is rebuilt |
| The schema-apply Job fails because views didn't finish building, naming the views and the nodes that haven't built them | The builds outlasted the view wait, `--view-wait-secs` (five hours by default, which the operator's Job uses; it ends before the Job's six hour limit). A Scylla node that is down never reports a view built, and a large base table can take longer. `thoradm schema apply` then records nothing and exits 1, which fails the Job without a retry | Bring the node back or remove it from the cluster (see the materialized views row in [Backends](./troubleshooting.md#backends)), then set the annotation below to a new value. When the builds are only slow, first run `thoradm --cluster-conf thorium.yml schema apply --view-wait-secs <secs>` with a longer wait yourself, then set the annotation; until the apply finishes, keep repo ingests held as above |

Run the schema apply again:

```bash
TC=$(kubectl -n thorium get thoriumcluster -o name)
kubectl -n thorium annotate $TC thorium.sandia.gov/retry-schema-apply="$(date +%s)" --overwrite
```

List the unacknowledged unexpected objects with their fingerprints, in the form
`driftAcknowledged` takes:

```bash
thoradm --cluster-conf thorium.yml schema diff --json | jq '[.backends[].objects[]
  | select(.kind == "unexpected" and (.acknowledged | not)) | {backend, object, fingerprint}]'
```

### Upgrade control

| Symptom | Cause | Fix |
|---------|-------|-----|
| The deployment upgrades on its own as soon as a new chart is deployed | `operator.cluster.upgrade.autoTargetDev` is on: it targets the operator's latest revision whenever no target is set | Never set it on a deployment you want to hold at a revision; use `targetRevision`, which alone decides how far an upgrade goes. The `thorium.sandia.gov/dev` namespace label only adds the development-cluster behaviour of `dangerousDowngradeLossy` |
| The phase stays `UpgradeRequired` after an upgrade | The target is older than the operator's latest revision | Raise `targetRevision` to the latest revision `thoradm migrate list` shows |
| `SchemaDrift` still lists objects that `thoradm schema diff` no longer shows (or shows none for new drift), and setting an annotation changes nothing | The operator reads drift only during a reconcile, once its last read is an hour old. A `Ready` cluster reconciles on a spec change, a component Deployment's change, an operator restart, or once a day; annotations other than `thorium.sandia.gov/retry-schema-apply` and `thorium.sandia.gov/allow-inline-config` start no reconcile | Restarting the operator (`kubectl -n thorium rollout restart deployment operator`) triggers a reconcile, which reads the drift only when a read is due: the last read (kept in the `thorium-upgrade-state` ConfigMap) is over an hour old, the acknowledgements changed, or a schema apply finished since it; until then `thoradm schema diff` shows the live state. Steps block on the drift the operator last read |
| `images-to-tools` (stage 3, from `2026-10-v03`) fails or retries with `the Scylla statements didn't prepare: ... unconfigured table scan_stats` (or `entities_by_name`) | On a plan from `2026-10-v03`, the conversions run before the image's schema is applied. The tool writer of an older image prepares every statement of its build, so it needs the objects `2026-10-v05` adds; a newer image's writer prepares only the objects the keyspace holds | Update the image to one whose writer starts without them; the Job's next attempt starts. With the older image, create the missing objects with their declared definitions in `cqlsh`, replacing `<namespace>` with `thorium.namespace` (below); the Job's next attempt starts, and the later schema apply finds them in place. Create `entities_by_name` only when `DESCRIBE MATERIALIZED VIEW <namespace>.entities_by_name` finds none |
| The event handler logs `Failed to start event controller; retrying` again and again after an upgrade, or the API logs `Skipped unreadable entity row` and a read of one entity answers `500` | An entity row whose stored data this build can't parse, such as a row a feature or development build wrote. Listings skip such a row and the API logs `Skipped unreadable entity row` with its `id`, `kind`, and `group`; a read of that entity by id or name answers `500` (`Entity <id> is unreadable: its stored data could not be parsed`). The event handler reads its rules from the API at startup and retries with a backoff that doubles from 1 to 60 seconds until it starts | Find the rows from the API's `Skipped unreadable entity row` lines, then delete those entities or rewrite their data in a form the build reads. Nothing else needs doing: the event handler starts once its startup reads succeed |
| Many finished `migration-check-*` Jobs in the namespace after a long drain | The operator runs a new check Job each time it checks a waiting step. It deletes a check Job once it has read its report, and Kubernetes deletes an unread one an hour after it ends, but a check Job an older operator made keeps that operator's retention of seven days; schema-apply, migration, and record Jobs are kept for seven days so their logs can be read, and `schema-status` Jobs are deleted once read | Nothing needs doing: Kubernetes deletes them when their retention ends. To delete finished check Jobs sooner: `kubectl -n thorium delete jobs -l thorium.sandia.gov/job-kind=migration-check --field-selector status.successful=1` |

The declared definitions of the objects `2026-10-v05` adds (`thoradm schema print --backend
scylla` prints every declared object):

```sql
CREATE TABLE IF NOT EXISTS <namespace>.scan_stats (year int, bucket int, snapshot_at timestamp, data text, PRIMARY KEY ((year, bucket), snapshot_at)) WITH CLUSTERING ORDER BY (snapshot_at DESC) AND default_time_to_live = 2592000;
CREATE MATERIALIZED VIEW IF NOT EXISTS <namespace>.entities_by_name AS SELECT name, group, kind, created, year, bucket, id, submitter, kind_data, description, image FROM <namespace>.entities WHERE name IS NOT NULL AND group IS NOT NULL AND kind IS NOT NULL AND created IS NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL AND id IS NOT NULL PRIMARY KEY ((name), group, kind, created, year, bucket, id) WITH CLUSTERING ORDER BY (group ASC, kind ASC, created ASC, year ASC, bucket ASC, id ASC);
```

The schema apply sets the `scan_stats` TTL from `thorium.events.stats_retention` when it differs.

### A drain that doesn't end

`drain-reactions` counts every reaction whose data hash in Redis says `Created` or `Started`, and
every registered worker without a tool instance; `thoradm migrate check --id drain-reactions`
prints the same counts and names neither. When `thorctl reactions get` (or `delete --status`)
finds none of what it counts, look in Redis (`redis-cli`, keys under `thorium.namespace`):

- **A reaction the listings can't see.** Find the hashes the drain counts, then delete each
  reaction by its id while the previous API still runs:

  ```bash
  redis-cli --scan --pattern '<namespace>:reaction_data:*' | while read -r key; do
    echo "$key $(redis-cli HGET "$key" status)"; done | grep -E 'Created|Started'
  # the key ends in <group>:<id>
  thorctl reactions delete --group <group> <id>
  ```

- **Stale worker registrations**, such as workers that stayed `Spawning` or name a cluster or node
  the scaler no longer has. Nothing removes a registration whose agent never ran or died without
  unregistering. List them, then delete them through the API under the scaler each names:

  ```bash
  redis-cli --scan --pattern '<namespace>:worker_data:*' | while read -r key; do
    [ "$(redis-cli HEXISTS "$key" tool_instance)" = 0 ] && echo "$key" && redis-cli HGETALL "$key"; done
  curl -X DELETE "<THORIUM_URL>/api/system/worker/<scaler>" -H "Authorization: token $TOKEN" \
    -H "content-type: application/json" -d '{"workers": ["<name>", "<name>"]}'
  ```

  `<scaler>` is the worker's `scaler` field (`K8s`, `BareMetal`, `Windows`, `Kvm`, or `External`),
  and `$TOKEN` an admin token as in [Logins](#logins).

### Logins

| Symptom | Cause | Fix |
|---------|-------|-----|
| A local account created by an older release can't log in (`401`) after the upgrade, although its password is right | Releases through 1.105.4 hashed passwords with argonautica's settings (argon2id with other costs and one lane per CPU of the API host), and builds that accept only the current argon2 settings refuse those hashes | Builds that accept the older settings verify such a hash, replace it with a hash of the current settings on the next successful login, and leave the user's tokens alone; restores of backups holding such hashes are accepted too. Nothing needs doing on those builds. On an image without that, an admin (an LDAP admin, for one) resets the password: `PATCH /api/users/user/<name>` with `{"password": "<new password>"}` (below) |
| The operator logs a `401` for its `thorium-operator` user in a loop, and the cluster never reaches `Ready` | The password in the `thorium-operator-pass` Secret doesn't verify for the user in Redis: the Secret was kept from another deployment or the data was restored or replaced, or the user's hash came from an older release and the API pods run an image that refuses older hash settings. The operator logs in through the API, so what counts is the image the API pods run: pods that weren't restarted after a rebuilt image was pushed under the same tag keep the image they pulled. The operator can reset its other users' passwords, but not its own | When the API pods run an image that refuses older hashes, restart them onto one that accepts them (`kubectl -n thorium rollout restart deployment/api`, with an image that has the fix), then restart the operator; the first login replaces the hash. Otherwise set the user's password from the Secret with another admin's token (below), then restart the operator (`kubectl -n thorium rollout restart deployment/operator`). After a restore into another `secret_key`, see [A Restore into Another secret_key](../thoradm/thoradm.md#a-restore-into-another-secret_key) |

```bash
TOKEN=$(printf '%s' '<admin token>' | base64 -w0)
# reset a user's password
curl -X PATCH "<THORIUM_URL>/api/users/user/<name>" -H "Authorization: token $TOKEN" \
  -H "content-type: application/json" -d '{"password": "<new password>"}'
# set the operator's own user from its Secret
PASS=$(kubectl -n thorium get secret thorium-operator-pass -o jsonpath='{.data.thorium-operator}' | base64 -d)
curl -X PATCH "<THORIUM_URL>/api/users/user/thorium-operator" -H "Authorization: token $TOKEN" \
  -H "content-type: application/json" -d "{\"password\": \"$PASS\"}"
```

### Results

The API refuses a result upload whose file names it can't store and serve back, with a `400`
naming the reason. A name is refused when it holds a backslash or an ASCII control character other
than a tab, is absolute, has an empty segment (`a//b`, a trailing `/`) or a segment of only dots
(`.`, `..`), or starts with `__thorium_`, the prefix reserved for Thorium's own files. Clients
name nested files with `/` on every platform.

| Symptom | Cause | Fix |
|---------|-------|-----|
| A job finishes but some result files are missing, and its logs hold `Warning: Skipping result file <path> since its name ...` | The tool wrote a file whose name the API refuses (a Linux tool writing `a\b` for a Windows archive path, for example). The agent checks each name with the same rules the API runs, skips a refused file, and uploads the rest | Have the tool write names the API accepts; the agent never renames files |
| A result upload fails with `400` and a reason such as `must separate directories with '/', not a backslash` | An agent or client from an older image that names files with the platform separator or doesn't skip refused names | Run the agent and thorctl that match the API |
| `thorctl results get` or `thorctl run` refuses to download a result file, naming it | A stored name that would land outside the output directory (absolute, or with `..` segments), which older APIs accepted | Nothing to fix in thorctl; delete the result if nothing needs it |

### Backups and restores

| Symptom | Cause | Fix |
|---------|-------|-----|
| `thoradm backup new` records the `redis` component as failed, writes no `redis.json`, and verify fails coverage (the API answered `422`, naming the records that don't parse) | A thoradm or API that fails the whole Redis backup on one record it can't parse: one built without the skip of unreadable records, or a thoradm newer than the API | Use the thoradm and API of the stage you are on. Otherwise the backup reports each record's Redis key: inspect it with `redis-cli` (`TYPE <key>`, then `HGETALL <key>` or `GET <key>`), fix or delete it, and run the backup again |
| `thoradm backup new` exits 2 with `skipped` `redis` records in `failures.jsonl` (any stage) | Redis records the API can't parse (an image hash missing its `group` field, for one), and the records that depend on them, are left out | Repair the records and back up again, or keep the backup: a restore of it needs `--accept-failures` and restores every other record ([Records that don't parse](../thoradm/taking_backups.md#records-that-dont-parse)) |
| Skipped rows with values the build doesn't know (an unknown enum variant, for example) | Rows written by an unreleased feature build that once ran against the keyspace | Delete them on the cluster if nothing needs them, or accept them: the backup holds everything else, and a restore needs `--accept-failures` |
| Result files skipped for their names | Result files whose stored names the backup can't keep as paths (an empty, `.`, or `..` segment), such as those left by validation or test runs. Other names the API now refuses at upload (see [Results](#results)), such as one with a backslash, are backed up | Accept them in the same way, or delete those results before the backup |
| `no Thorctl config at '<path>'` (the latest release's thoradm, stage 3; the stage-1 and stage-2 thoradm fail with a bare file-not-found error naming the path) | Backup, restore, and `verify --cluster` reach the API with a Thorctl config, by default `~/.thorium/config.yml` (root's home under `sudo`) | Run `thorctl login` as an admin, or pass `--ctl-conf <path>` before the subcommand |
| Another admin can't resume or read a backup | A backup directory is private to the user who ran thoradm unless it has the setgid bit (and always with the stage-1 and stage-2 thoradm) | With the latest release's thoradm (stage 3), create it shared: `install -d -m 2770 -g <admins> DIR`, or `chgrp <admins> DIR && chmod 2770 DIR` before the backup. With the stage-1 or stage-2 thoradm, use a directory only the user running it can read, and run every step as that user |
| Every restored password login fails with `401` | The target's `thorium.secret_key` differs from the source's | See [A Restore into Another secret_key](../thoradm/thoradm.md#a-restore-into-another-secret_key) |
| A plain restore refuses a non-empty target | The fresh chart install's toolbox import wrote rows | Install with `operator.toolbox.enabled=false`, or restore with `--merge` or `--wipe` ([Merge or Wipe](../thoradm/thoradm.md#merge-or-wipe)) |
