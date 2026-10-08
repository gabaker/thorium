# Upgrading Thorium

This page explains how to upgrade a Kubernetes deployment of Thorium: moving to a new Thorium
version, and the revision upgrades the operator runs when a new version changes how Thorium is
deployed. It is the reference for every revision's upgrade steps.

## Overview

An upgrade has two parts:

1. **A new version.** Deploy the new charts (or operator image) with the same values. The new
   operator applies its own `ThoriumCluster` CRD when it starts (Helm never updates the CRD in a
   chart's `crds/` directory).
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

  `thoradm migrate` needs no cluster config, so any `thoradm` binary of the new version works,
  for example one downloaded from `<THORIUM_URL>/api/binaries/linux/x86-64/thoradm`.

## Where a deployment starts

The operator records the revision in the `thorium-upgrade-state` ConfigMap in the thorium
namespace. The first time a new operator sees a namespace without that ConfigMap, it decides the
starting revision:

| Situation | Recorded revision |
|-----------|-------------------|
| The Secret `thorium-legacy-cluster` exists (a deployment converted by `convert-to-helm.sh` that lost its state) | `2026-10-v01` |
| No operator has run in the namespace (no `thorium-operator-pass` Secret): a fresh install | The operator's latest revision |
| The `ThoriumCluster` carries the annotation `thorium.sandia.gov/allow-inline-config: "true"` and names no `config_secrets` (a pre-Helm deployment kept outside Helm) | `2026-10-v01` |
| Anything else (a Helm deployment from before revisions were recorded) | `2026-10-v02` |

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

Put the target in your values file so later upgrades keep it, and raise it on every upgrade that
brings a new revision: a deployment that reaches a target older than the operator's latest
revision waits in `UpgradeRequired` again ("set spec.upgrade.target_revision to <latest> to
continue").

A target that can't be used sets the phase to `Error`:

| Message | Cause |
|---------|-------|
| `Revision "<id>" is not a revision id like 2026-10-v02` | The target isn't in the `YYYY-MM-vNN` format (the CRD also rejects it) |
| `Revision <id> is not known to this Thorium build (latest is <latest>)` | The target is newer than this operator's latest revision, or not in its catalog |
| `The target revision <target> is older than this cluster's revision <current>; upgrades never move backwards` | Remove or raise the target |
| `This cluster is at revision <current>, which is newer than this Thorium build's latest revision <latest>; ...` | The operator is older than the deployment: deploy the newer Thorium again (downgrades aren't supported) |
| `This cluster's upgrade state (schema version <n>) was written by a newer Thorium operator ...` | Same as above |

## How steps run

Once a target is set, the operator runs every step between the deployment's revision and the
target, in order, in the `Upgrading` phase. After each step it records the outcome in
`thorium-upgrade-state`, so an interrupted upgrade resumes where it stopped. When the target is
the latest revision, it then reconciles the deployment as usual and deploys the new components.

| Step kind | Runs |
|-----------|------|
| `Config`, `Infra` | Automatically once a target is set |
| `Data`, `Manual` | Only with an approval (see [Approvals](#approvals)), and only when the step has work to do; a step with nothing to do is recorded as `Skipped` without one |

A step waiting on a backend (for example Scylla not accepting connections yet) is checked again
every 15 seconds; a step blocked on an admin is checked again every minute. While a step is
blocked or waiting, the operator changes no component Deployments, so manual work such as
scaling a component down isn't undone. A failed step is reported in `status.upgrade.steps` and
retried.

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

Approve it by naming the backup you took (an approval with an empty `backup` doesn't count):

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
as `spec.upgrade.approvals` (`[{step, backup}]`).

The operator never takes backups itself; `backup` only records where yours is. Some steps can't be
run by the operator at all: once approved, such a step reports "Waiting for manual work" with the
procedure to follow. When the work is done the operator notices on its next check, records the
step as `Done` with the message "fixed manually", and continues.

## Inspecting revisions

`thoradm migrate` reads the revision catalog built into thoradm and needs no cluster config:

```bash
thoradm migrate list                          # every revision and its steps
thoradm migrate plan --from 2026-10-v01       # the steps from a revision to the latest
thoradm migrate plan --from 2026-10-v01 --to 2026-10-v02
```

`thoradm migrate run <id>` and `thoradm migrate verify <id>` print the manual procedure of a data
migration thoradm can't run yet and exit with code 3.

## Limitations

- The operator never runs `Data` or `Manual` work itself and never takes backups. It doesn't
  scale down the components a step lists under `quiesce` (that list is informational), and it
  doesn't start thoradm migrations.
- Revisions never move backwards, and downgrades aren't supported. Rolling the operator image back
  also rolls the cluster-wide `ThoriumCluster` CRD back.

## Rolling back

- **Before you set a target**, nothing has changed: deploy the previous chart version (or
  operator image) again.
- **After steps have run** and the new components rolled out, restore from your backups onto the
  previous version.

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
| `elastic-identity` | Infra | Checks that Thorium's Elasticsearch user authenticates and holds the index privileges Thorium needs, creating it first when the Elasticsearch bootstrap is set |
| `elastic-reindex-keyword-mappings` | Data | Skipped when every existing index maps `group` as a keyword; otherwise needs an approval and then the manual [reindex](#reindexing-elasticsearch) |
| `finalize` | Config | Records the new revision |

megathor deployments made before the indexes were left to the search streamer created
`thorium_repo_results` and `thorium_repo_tags` without mappings, so their `group` field is `text`
and group filters can miss documents; those deployments need the reindex. They also created
`thorium_file_results`, `thorium_file_tags`, and `thorium` indexes that Thorium never uses, which
are safe to delete.

## Reindexing Elasticsearch

Elasticsearch holds only data the search streamer can rebuild from Scylla. To rebuild indexes
whose mappings are wrong (the `elastic-reindex-keyword-mappings` step names them in
`status.upgrade.steps`):

1. Approve the step (see [Approvals](#approvals)) with a reference to your backup. The step then
   waits for manual work.
2. Stop the search streamer **before** deleting anything: Elasticsearch recreates a deleted index
   with dynamic mappings if anything writes to it.
3. Delete each index the step names, as the Elasticsearch superuser.
4. Start the search streamer again. It recreates every missing index with the right mappings and
   streams every result and tag from Scylla into it.

```bash
kubectl -n thorium scale deployment search-streamer --replicas=0
kubectl -n thorium wait --for=delete pod -l app=search-streamer --timeout=120s
# with the chart's Elasticsearch; use your own endpoint and superuser otherwise
PASS=$(kubectl -n elastic get secret elastic-es-elastic-user -o jsonpath='{.data.elastic}' | base64 -d)
kubectl -n elastic port-forward svc/elastic-es-http 9200 &
curl -sk -u "elastic:$PASS" -X DELETE https://localhost:9200/thorium_repo_results
curl -sk -u "elastic:$PASS" -X DELETE https://localhost:9200/thorium_repo_tags
kubectl -n thorium scale deployment search-streamer --replicas=1
```

The operator rechecks the mappings within a minute, records the step as `Done` ("fixed
manually"), and continues. Searches return incomplete results until the search streamer has
streamed everything back.

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

- **minithor** sets `autoTargetDev`, so `minithor deploy` upgrades the deployment to the latest
  revision. It stops with an error when a step is blocked; pass the approval with
  `--set 'operator.cluster.upgrade.approvals[0].step=<step>' --set 'operator.cluster.upgrade.approvals[0].backup=<where>'`
  or a `--values` file and deploy again.
- **megathor** accepts `UpgradeRequired` as a finished run and prints the planned steps; set
  `thorium_upgrade_target_revision` (and `thorium_upgrade_approvals`) and run the playbook again.
  It fails at once when a step is blocked or the phase is `Error`.
- **Without Helm**: change the operator image and `spec.version` together, then set
  `spec.upgrade.target_revision` (see
  [Deploy the Operator Without Helm](./deploy-thorium.md#upgrading)).
