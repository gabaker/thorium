# Troubleshooting Deployments

This page is organized by symptom. Each section quotes the message you see (in the
`ThoriumCluster` status, the operator log, or Helm's output) so you can search for it, and says
what to do. Placeholders such as `<name>` stand for values from your deployment. Replace the
`thorium` namespace with `<prefix>-thorium` when you use a [namespace
prefix](./concepts.md#namespaces-and-the-namespace-prefix).

## Where to look

```bash
# phase, revision, and status message
kubectl -n thorium get thoriumcluster -o wide
# the full status, including the upgrade steps and recent events
kubectl -n thorium describe thoriumcluster
# the steps left to run and their state
kubectl -n thorium get thoriumcluster -o jsonpath='{range .items[*].status.upgrade.steps[*]}{.revision} {.step} {.state}: {.message}{"\n"}{end}'
# what the operator is doing
kubectl -n thorium logs deploy/operator
# the component pods
kubectl -n thorium get pods
```

`Provisioning` usually resolves on its own; only `Error`, a blocked upgrade step, and
`UpgradeRequired` need you. A status whose `observed_generation` is older than the cluster's
`metadata.generation` hasn't caught up with your latest change yet. See [Operating a
Deployment](./operate.md#status) and the [status reference](./thoriumcluster.md#status).

### Reading the operator's logs

Each operator log line starts with a timestamp, the level, and the steps it happened in,
outermost first, each with its fields, then the module that logged it, the message, and the
message's own fields:

```text
2026-11-04T16:53:15.070067Z  INFO reconciling object{object.ref="ThoriumCluster.v1.sandia.gov/thorium.thorium" object.reason="object updated"}:ThoriumCluster::reconcile{cluster="thorium" namespace="thorium" generation=4}:operate::roll_api:deployments::deploy{component="api"}: thorium_operator::k8s::deployments: msg="Deployment created" deployment="api"
```

The steps say where the operator is: `reconciling object{object.ref=... object.reason=...}` is
added by the Kubernetes controller runtime around every reconcile and says why it ran (`object
updated`, `reconciler requested retry`, `error policy requested retry`, and so on),
`ThoriumCluster::reconcile` is one pass over a cluster (`Reconciled ThoriumCluster` at its end
says when it is looked at next), `upgrades::gate` and
`upgrades::run{start=... target=...}` with `upgrades::run_step{step=... revision=...}` are an
upgrade, `schema_apply::*`, `migration::*`, and `record::*` are the thoradm Jobs, `operate::prepare`
and `operate::check_backends` are the backends, `deployments::deploy{component=...}` and
`deployments::wait_for_rollouts` are the components, and `Node::reconcile{node=...}` and
`nodes::*` are node provisioning. A failed reconcile is logged as `Controller error` with its
causes, followed by when it is retried; these two lines carry only the `reconciling object` step,
since the controller runtime logs them after the reconcile has ended:

```text
ERROR reconciling object{object.ref="ThoriumCluster.v1.sandia.gov/thorium.thorium" object.reason="object updated"}: thorium_operator::k8s::watchers::thorium_cr: msg="Controller error" cluster="thorium" namespace="thorium" error="Error: Failed to list ThoriumClusters: ..."
 INFO reconciling object{object.ref="ThoriumCluster.v1.sandia.gov/thorium.thorium" object.reason="object updated"}: thorium_operator::k8s::watchers::thorium_cr: msg="Requeuing ThoriumCluster reconciliation" cluster="thorium" requeue_secs=60
```

To see more, raise `operator.controller.logLevel` (`Off`, `Error`, `Warn`, `Setup`, `Info`,
`Debug`, or `Trace`; default `Info`; `Setup` logs the same as `Warn` in the operator), or pass
`--log-level` (see [Operator command line](./thoriumcluster.md#operator-command-line)). `Debug`
and `Trace` also show the internals of the libraries the operator uses (the Kubernetes and HTTP
clients and the Scylla driver), which are verbose, so lower the level again when you are done.
Requests that carry a password (Scylla role statements, Elastic and Thorium user requests) are
never recorded at any level. Set `NO_COLOR=1` on the operator Deployment to drop the color codes
from the levels; the chart has no value for it, so a Helm upgrade removes it again. The
operator's `--trace <file>` flag (or `THORIUM_OPERATOR_TRACE`) takes a tracing config in the
format the other components use, which can also send its traces to a gRPC (OTLP) collector.

The Thorium image is published for linux/amd64 and linux/arm64 under one tag, and each node pulls
its own architecture. Only the amd64 image includes `curl`, `jq`, and `lsof`. To use them in a
pod on an arm64 node, attach a debug container that has them:

```bash
# a shell sharing the pod's namespaces, in an image that has the tools you need
kubectl -n thorium debug -it <pod> --image=<debug image> --target=<container>
```

## The operator refuses or holds the cluster

### "has not been converted"

> This ThoriumCluster was deployed by the minithor or megathor scripts from before the Helm
> charts and has not been converted (spec.config holds thorium.secret_key inline; the
> ThoriumCluster has no config_secrets). The operator changed nothing. ...

The cluster keeps `thorium.secret_key` inline in `spec.config` and names no `config_secrets`, which
is how the pre-Helm scripts wrote it. The operator changes nothing and checks again every 10
minutes. Either convert it to Helm ([Converting a Pre-Helm Deployment](./convert-to-helm.md)) or
keep it without Helm ([Moving a pre-Helm deployment without
Helm](./deploy-thorium.md#moving-a-pre-helm-deployment-without-helm)), which sets the
`thorium.sandia.gov/allow-inline-config: "true"` annotation.

### "Thorium supports one deployment per Kubernetes cluster"

> Thorium supports one deployment per Kubernetes cluster; ThoriumCluster `<ns>/<name>` is already
> deployed; namespace prefixes only rename namespaces. The operator changes nothing for this
> ThoriumCluster; delete it or delete `<ns>/<name>` first ...

The operator manages only the oldest `ThoriumCluster` it sees. Delete the extra one (deleting it
doesn't touch the deployed cluster), or remove the deployed one first. See [One deployment per
Kubernetes cluster](./thoriumcluster.md#one-deployment-per-kubernetes-cluster).

### Phase `UpgradeRequired`

> This cluster is at revision `<current>` and this operator deploys revision `<latest>`. Nothing is
> changed until spec.upgrade.target_revision is set to `<latest>` (Helm value
> operator.cluster.upgrade.targetRevision); the steps that will run are listed in
> status.upgrade.steps

> This cluster is at its target revision `<target>`, but this operator deploys revision
> `<latest>`; set spec.upgrade.target_revision to `<latest>` to continue

> This cluster reached its target revision `<target>`, but this operator deploys revision
> `<latest>`; set spec.upgrade.target_revision to `<latest>` to continue

The operator is newer than the cluster's revision. Components keep running their current
version, and node provisioning pauses, until you set a target. Review `status.upgrade.steps` and
set the target as described in [Setting a target](./upgrades.md#setting-a-target).

### "stopped before it finished"

> ... Step `<step>` of revision `<revision>` stopped before it finished (migration Job `<job>`),
> and the components the upgrade paused stay at zero replicas: `<component>` (`<count>`), ... To
> go on, set spec.upgrade.target_revision to `<revision>` or a later revision again, and the
> upgrade resumes from where it stopped; to go back, restore the backup step `<step>`'s approval
> names (`<backup>`). Don't scale the paused components up by hand

When the stopped step is part of a downgrade, the message says "`<revision>` or an earlier one"
instead, since a downgrade goes on under a target at or below the step's revision.

The target was cleared, set to the cluster's revision, or set to one the operator can't use while
a migration step ran, so the operator stopped the step's Job (see [How steps
run](./upgrades.md#how-steps-run)). While the Job's pod writes its checkpoint the message ends
"Waiting for the migration Job `<job>` to stop and write its checkpoint". A hold between steps
says "The upgrade stopped between steps" and names the step that paused the components. The
components stay at zero; don't scale them up by hand, since the older pods would run on partly
converted data. Set the target again to resume, or restore the backup the message names. A message
that adds "The operator can't stop the migration Job of the open run until the cluster's config
resolves" names a config error to fix first (see [Config errors](#config-errors)); the operator
tries again every minute.

### Invalid or impossible targets

| Message | Cause and fix |
|---------|---------------|
| Revision `"<raw>"` is not a revision id like 2026-10-v02 or 2026-10-v02-pre.`<change>` | The target isn't `YYYY-MM-vNN` (or a feature branch's `YYYY-MM-vNN-pre.<change>`). Fix the value |
| Revision `<rev>` is not known to this Thorium build (latest is `<latest>`) | The target is newer than this operator, or a revision that doesn't exist. Use a revision from `thoradm migrate list` of the same build |
| This Thorium build's catalog has no path from revision `<current>` to revision `<target>` | The target can't be reached from the cluster's revision. A target older than it needs reverse edges all the way down (`thoradm migrate list` marks an edge without one `one-way`); otherwise set the target to the current or a newer revision, or restore from a backup |
| Step `<step>` of revision `<revision>` can't be undone, so this downgrade is refused before anything runs; restore from backup instead ... | A step between the cluster's revision and the older target has no reverse. Keep the target, or restore a backup taken before that step ran |
| Step `<step>` of revision `<revision>` finished on this cluster, but no step on the path down to the target undoes it ... | Downgrade one revision at a time, along the edges the cluster took (see [Downgrading](./upgrades.md#downgrading)), or restore from a backup |
| This cluster is at revision `<current>`, which is newer than this Thorium build's latest revision `<latest>`; with the Thorium build that knows revision `<current>` still deployed, revert the cluster to `<latest>` ... | The operator was rolled back past a revision it doesn't know. Deploy the newer operator again, revert the cluster with it as the message says (lower the target, or `thoradm migrate revert --to <latest>`), and then deploy the older one (see [Downgrading](./upgrades.md#downgrading)); or, when the newer operator refuses the revert as irreversible, restore from a backup taken before the step it names |
| This cluster's upgrade state (schema version `<n>`) was written by a newer Thorium operator than this one ... | The `thorium-upgrade-state` ConfigMap was written by a newer operator. Deploy that operator again and revert the cluster with it |
| This cluster is at pre-release revision `<id>`, which this Thorium build doesn't know; revert it with the image of the branch that applied it ... | A development cluster ran a feature branch's pre-release revision. Deploy that branch's image again, set the target to the base release the message names (or run `thoradm migrate revert --to <base>`), and deploy this build once the cluster is back at it |
| This cluster is at revision `<current>`, older than `<minimum>`, the oldest revision this Thorium release upgrades from; install Thorium `<intermediate>` first ... | The deployment is too old for this release to upgrade directly. Deploy the named Thorium release, let it finish upgrading, then deploy this one; older deployments follow the table of valid upgrade paths in [Upgrading Thorium](./upgrades.md) |

These hold the cluster in `Error` and are checked again every hour or when the spec changes.

### "is blocked at step elastic-reindex-keyword-mappings"

> The upgrade to `<target>` is blocked at step elastic-reindex-keyword-mappings, which needs an
> approval naming a backup. Its full message is in the ThoriumCluster's status.upgrade.steps.

The step's check Job found Elasticsearch indexes that are missing or mapped differently from the
ones this release declares, and the step needs an approval naming your backup. Add
`{step: elastic-reindex-keyword-mappings, backup: <where your backup is>}` to
`spec.upgrade.approvals` (Helm `operator.cluster.upgrade.approvals`). An approval whose `backup` is
empty doesn't count, and the message says so ("names no backup"). See
[Approvals](./upgrades.md#approvals). Once it is approved, the operator scales the search streamer to
zero and runs the `elastic-rebuild` migration Job, and the search streamer refills the indexes after
the rollout (see [Reindexing Elasticsearch](./upgrades.md#reindexing-elasticsearch)). A Job that
fails because Thorium's Elasticsearch user may not delete the indexes needs `delete_index` granted
on them; its logs are in `status.upgrade.failure`. minithor and megathor stop waiting with an error
as soon as a step is `Blocked`.

### The `elastic-identity` step keeps failing

A step `elastic-identity` that keeps failing with a TLS error about the certificate, such as
`certificate verify failed`, means the current components verify Elasticsearch's certificate and
the config doesn't let them: pre-Helm deployments stored `insecure_certificates: false` although
their components never checked. Skip verification for ECK's self-signed certificate with the
`elastic` setting `insecure_certificates: true`, or verify against a CA with `elastic_ca_secret`;
see step 4 of [Moving a pre-Helm deployment without
Helm](./deploy-thorium.md#moving-a-pre-helm-deployment-without-helm) and [Elasticsearch
certificates](./helm-configuration.md#elasticsearch-certificates). A pre-Helm deployment kept
outside Helm also needs the `thorium.sandia.gov/allow-inline-config: "true"` annotation from step 3
of that procedure, or the operator treats it as unconverted and changes nothing.

### A step is blocked by schema drift

> Step `<step>` targets schema objects that differ from the recorded revision: scylla
> `<object>` (unexpected). Restore them to the recorded definition, or acknowledge an unexpected
> object in spec.upgrade.drift_acknowledged; thoradm schema diff has the details

A step that targets a Scylla, Redis, or S3 object that is `changed` or `unexpected` waits until the
object matches the recorded revision again or, for an `unexpected` one, you acknowledge it. A
`missing` object, a `changed` Elasticsearch index, and a changed Redis layout the step itself
converts don't block it. When more than 200 objects past the first 20 would block, every step with
targets waits. Run `thoradm schema diff` for the full list. See [Schema
drift](./upgrades.md#schema-drift).

### Conditions in `status.upgrade.conditions`

| Condition | What to do |
|-----------|------------|
| `SchemaDrift` | The live schema differs from the recorded revision. `thoradm schema diff` lists the objects; restore them or acknowledge `unexpected` ones (see [Schema drift](./upgrades.md#schema-drift)) |
| `MigrationIssues` | A migration finished with issue groups. `thoradm migrate issues --id <migration>` lists them; fix the objects and run `thoradm migrate run --id <migration> --reason <code>` (see [Data migrations](./upgrades.md#data-migrations)) |
| `SearchIncomplete` | Search results are incomplete until the search streamer finishes reingesting from Scylla. It clears itself; the message names the sources still reingesting (see [Rebuilding Elasticsearch indexes](./upgrades.md#rebuilding-elasticsearch-indexes)) |
| `LossySettingRefused` | `dangerousDowngradeLossy` is set but the cluster isn't a development cluster (`autoTargetDev` and the `thorium.sandia.gov/dev=true` label on the namespace). Unset it or label the namespace; lossy steps need their own approvals meanwhile (see [Accepting loss](./upgrades.md#accepting-loss)) |
| `CatalogSkew` | The image's thoradm knows another revision catalog than the operator, so no data-changing step is planned. It can show for a short time while the operator and the image run different builds, such as during a `helm upgrade` that changes both; if it stays, deploy the operator and the image of the same release |
| `ReactionsResubmitted` | `False`: the resubmission of the requests an approved drain cancel recorded is running, or its last Job failed (the message names it and its counts). After three Jobs inside the conversion pause, set the `thorium.sandia.gov/refresh-check` annotation to a new value to run it again, and read `thoradm migrate report --id resubmit-reactions`. `True`: it finished |
| `SchemaSkew` | The schema-apply Job reported a schema fingerprint other than the one the operator's build declares; the rollout continues. Deploy the operator and the image of the same release |
| `MutableImageTag` | The cluster runs a mutable image tag such as `latest`, so a new image under the same tag doesn't run the schema apply again. Use a version tag, or set the `thorium.sandia.gov/retry-schema-apply` annotation on the `ThoriumCluster` to a new value after pulling a new image |

### A lossy downgrade step stays blocked

> Step `<step>` loses data: ... To accept the loss, take a backup and add the approval {step:
> `<step>`, backup: `<where your backup is>`, acceptLoss: true, confirm: `<code>`} ...

> The approval for `<step>` was refused: ... Repeat the current code `<code>` as confirm

The step needs an approval with `acceptLoss: true` (`accept_loss` in a `ThoriumCluster` without
Helm) and the code it shows now. A code belongs to one block of one step and is used up when the
step runs, so a failed run needs a new approval with the new code. When the message says a field
"isn't covered by thoradm backup", add `acceptBackupGaps: true` (`accept_backup_gaps`) to accept
that a backup can't restore it. See [Accepting
loss](./upgrades.md#accepting-loss).

### The API refuses to start

The API checks the schema applied to Scylla when it starts. It exits, and the pod restarts, with
one of these messages.

| Message | Cause and fix |
|---------|---------------|
| the Thorium schema isn't applied: the keyspace, the schema_migrations table, or the deployment's record doesn't exist; run `thoradm schema apply` | Nothing was applied to this Scylla. With the operator the schema-apply Job does it before components roll out; otherwise run `thoradm schema apply` |
| the applied schema `<fingerprint>` is older than the one this build needs, `<fingerprint>`: the operator hasn't applied revision `<revision>` yet | The new image started before its schema was applied. Wait for the operator's schema-apply Job to finish, or run `thoradm schema apply` |
| the applied schema `<fingerprint>` is revision `<revision>`, which changes what this build ... reads incompatibly (`<objects>`); roll the deployment forward to a build that knows it | A newer release applied its schema and the deployment went back to an older image. Roll forward, or downgrade the revision first (see [Downgrading](./upgrades.md#downgrading)) |
| the applied schema `<fingerprint>` is in no revision this build knows ...; a newer build probably applied it | Same cause with a build this one has never heard of. Roll forward, or compare the two with `thoradm schema diff` |
| `<setting>` differs from the value recorded for this deployment while data shaped by it exists; set it back to the recorded value | A data-shaping setting (such as a partition size) changed. Put the old value back, or let the operator move the data (see [Changing a partition size](./upgrades.md#changing-a-partition-size)) |
| the schema record couldn't be read: ... | Scylla failed the read; the API tries again. See the Scylla pods |

### thoradm refuses a record a newer build wrote

`thoradm schema apply` and `thoradm migrate run` refuse, with exit 1 and nothing changed, to write
behind a newer build, the same way the API refuses to start on a schema it doesn't know:

| Message | Cause and fix |
|---------|---------------|
| This thoradm can't apply the schema: revision `<revision>` is later than this build knows (the keyspace records revision `<revision>` with schema fingerprint `<fingerprint>`; this build's catalog ends at revision `<revision>` with schema fingerprint `<fingerprint>`) | A newer release recorded its revision and this thoradm is older, as from an older image or chart run by hand. Applying it would record the older fingerprint over the newer revision, let the older API start, and stop the newer one. Use the thoradm of the release that recorded the revision, downgrade through the Helm chart and the operator (see [Downgrading](./upgrades.md#downgrading)), or restore a backup. `is unknown to this build` means the revision comes from another branch |
| This thoradm can't apply the schema: schema fingerprint `<fingerprint>` is unknown to this build (the keyspace records no revision ...) | A newer build applied its schema to a keyspace with no recorded revision. The same fixes apply |
| This thoradm can't run a migration: revision `<revision>` is later than this build knows (...) | Any `migrate run` over a newer build's record. The same fixes apply |
| This thoradm can't run step `<revision>/<step>`: ... the plan from revision `<revision>` doesn't include it | The step belongs to a revision the deployment already moved past, the record shows it finished, or the planner refuses its path. Re-run a finished step's issue groups with `--reason <code>` or `--objects <keys>`, and move the deployment with its target revision (see [Data migrations](./upgrades.md#data-migrations)) |

## Config errors

| Message | Cause and fix |
|---------|---------------|
| config secret `<name>` not found in `<ns>` | A `config_secrets` entry names a Secret that doesn't exist in the cluster's namespace |
| config secret `<name>` in `<ns>` has no key `<key>` | The Secret exists but lacks the key (`thorium.yml` by default) |
| bootstrap admin secret `<name>` not found in `<ns>` | A bootstrap Secret is missing |
| Secret `<name>` in `<field>` collides with a Secret the operator manages; rename it (reserved: thorium, keys, keys-kaboom, docker-skopeo, registry-token, thorium-image-pull, and any name ending in -pass) | Rename your Secret |
| Config secret `<ns>/<name>` is not valid YAML: ... | Fix the document in the Secret |
| Invalid Thorium config from spec.config merged with config secrets [...]: ... | The merged config doesn't match Thorium's config types: a wrong type, a quoted number, or a YAML tag such as `!Grpc`. See [Config merging](./thoriumcluster.md#config-merging) |
| Invalid thorium.namespace in the Thorium config from spec.config and its config secrets, so nothing was rolled out: ... | `thorium.namespace` names the Scylla keyspace and prefixes the Redis keys, so it holds only lowercase letters, digits and underscores, starts with a letter, and is at most 48 characters. Fix it in `spec.config` or the config Secret that sets it; nothing was deployed with it. See [Namespaces](./helm-configuration.md#namespaces) |

## Backends

| Message | Cause and fix |
|---------|---------------|
| Waiting for Scylla to accept connections as `<who>`: ... | Scylla isn't up yet (phase stays `Provisioning`, checked every 15 seconds). If it persists, check the Scylla pods and `scylla.nodes` |
| Could not log in to Scylla with the default cassandra/cassandra superuser, so Thorium's Scylla role can't be created. Set bootstrap.scylla.admin_secret to a Secret holding Scylla superuser credentials: ... | The default superuser was dropped or never existed. Set `operator.cluster.bootstrap.scylla.adminSecret` (`spec.bootstrap.scylla.admin_secret`), or pre-create Thorium's role and set its credentials. See [External services](./helm-configuration.md#external-services) |
| bootstrap.scylla.admin_secret `<name>` is missing and Thorium's role can't log in | Recreate the admin Secret, or fix Thorium's Scylla credentials |
| Waiting for Elastic to accept Thorium's credentials (user `<user>`): ... | With the chart's Elasticsearch, ECK is still applying Thorium's file-realm user; this clears within a minute or two, and the operator reports an error if it lasts over 5 minutes. With an external Elasticsearch, create the user or set `bootstrap.elastic` |
| Elastic `<node>` rejected Thorium's credentials (user `<user>`): ... | An external Elasticsearch has no such user or a different password. Create it with the privileges Thorium needs, or set `operator.cluster.bootstrap.elastic.adminSecret` |
| Elastic `<node>` has rejected Thorium's credentials (user `<user>`) for over 5 minutes: ... | An in-cluster ECK Elasticsearch (`*-es-http`) kept rejecting Thorium's user for longer than ECK's file realm takes to apply it, so the password in Thorium's config is likely wrong for it, as with the ECK of a converted pre-Helm deployment. Set the password the user has there, create the user with that password, or set `operator.cluster.bootstrap.elastic.adminSecret`; the operator retries on its own |
| bootstrap.elastic.admin_secret `<name>` is missing and Thorium's Elastic user can't authenticate with the privileges it needs | Recreate the admin Secret, or fix Thorium's Elastic user |
| Elastic user `<user>` is missing index privileges Thorium needs: ... | Grant `view_index_metadata`, `write`, and `read` on every Thorium index, plus `create_index` on indexes that don't exist yet (`all` on `thorium*` covers the defaults) |
| Failed to `<request>` at Elastic `<node>`: ... certificate ... | TLS verification failed. For the chart's Elasticsearch keep `operator.backends.elastic.insecureCertificates: true`; for an external one set `caSecret` (and check the hostname in `node` matches the certificate). See [Elasticsearch certificates](./helm-configuration.md#elasticsearch-certificates). Right after a CA rotation this clears on its own within about a minute |
| Waiting for Elastic `<node>` to accept connections: ... / Waiting for Elastic to become available: ... | Elasticsearch isn't up yet |
| Waiting for S3 at `<endpoint>` to accept connections: ... | The S3 store isn't reachable yet |
| Failed to create bucket `<bucket>` at `<endpoint>` (thorium.s3.region: `<region>`): ... | S3 refused to create the bucket. Check that Thorium's S3 identity may create buckets and that `thorium.s3.region` is the store's region (unset, empty, or `us-east-1` sends no location constraint). Or pre-create the bucket: the operator skips buckets that exist. See [External services](./helm-configuration.md#external-services) |
| skip_bucket_auto_create is set but `<n>` required S3 bucket(s) are missing or inaccessible at `<endpoint>`: ... | Create the listed buckets or grant Thorium's S3 identity access to them |
| Waiting for Redis at `<addr>` to accept connections: ... | Redis isn't up yet, or `redis.host`/`port` is wrong |
| The schema-apply Job failed with "the materialized views didn't finish building within `<secs>`s, so nothing was recorded: `<view>` (not built on `<address> (<host id>)`) ..." | A Scylla node that is down, or still building, never reported the views built, and the Job's view wait (five hours) passed. Bring the node back, or remove it from the cluster (`nodetool removenode`), then set the `thorium.sandia.gov/retry-schema-apply` annotation on the `ThoriumCluster` to a new value. When every node is up and the build is only slow, run `thoradm schema apply --view-wait-secs <secs>` by hand with a longer wait (see [The Declared Schema](../thoradm/thoradm.md#the-declared-schema)) |
| Elastic index `<name>` does not map group as a keyword, so group filters may not ... (alongside `Ready`) | The index was created from dynamic mappings. Reindex it: see [Reindexing Elasticsearch](./upgrades.md#reindexing-elasticsearch) |

When the operator reaches the Kubernetes cluster through a context its config doesn't list, the
operator log shows:

> The operator reaches this Kubernetes cluster through kube context '`<context>`', but
> thorium.scaler.k8s.clusters only has config for [...], so its nodes are not provisioned or
> labelled for Thorium. ...

Key the cluster by that context (Helm value `operator.cluster.scaler.k8s.context`). An operator
running in the cluster always uses `kubernetes-admin@cluster.local`.

### The schema probe reports "error sending request"

> Transient Error: the probe failed to read GET thorium_sample_results/_mapping: error sending
> request for url (https://elastic-es-http.elastic-system.svc.cluster.local:9200/...): ...

The operator's `thoradm schema status` Job, or a migration Job, couldn't reach Elasticsearch. The
text after the URL names the cause:

| Cause in the message | Meaning |
|----------------------|---------|
| `dns error` or `failed to lookup address` | The name in `elastic.node` doesn't resolve from the Thorium namespace |
| `Connection refused` | The name resolves but nothing listens on the port: Elasticsearch is down, or the port is wrong |
| `timed out` | Packets are dropped on the way, most often by a NetworkPolicy in the backend's namespace (a CNI that rejects instead of dropping shows `Connection refused`) |
| `invalid peer certificate` or another certificate error | TLS verification failed; see the `certificate` row in [Backends](#backends) |
| A proxy's error, or the request reaches a proxy host | The Job's request went through a proxy: set `no_proxy` in the API's `env` (the Jobs take the API's proxy settings); see [Proxies](./helm-configuration.md#proxies) and [Upgrade Paths](./upgrade-paths.md#proxies-and-network-policies) |

These Jobs run in the Thorium namespace with the labels `app.kubernetes.io/managed-by:
thorium-operator` and `thorium.sandia.gov/job-kind: <kind>`, not `app: api`, so a NetworkPolicy
that admits only the API's pods lets the API reach Elasticsearch while the Jobs time out. To see
what a Job sees, start a pod with the same labels and try the request from it, using the Thorium
image (on amd64, where it has `curl`) or another image with `curl` and `getent`:

```bash
kubectl -n thorium run probe-check --rm -it --restart=Never --image=<thorium image> \
  --labels=app.kubernetes.io/managed-by=thorium-operator,thorium.sandia.gov/job-kind=schema-status \
  --command -- sh -c '
    # does the name resolve?
    getent hosts elastic-es-http.elastic-system.svc.cluster.local
    # does the port answer, and does TLS verify? (-k skips verification, as insecureCertificates does)
    curl -sS -k -o /dev/null -w "%{http_code}\n" --max-time 10 \
      https://elastic-es-http.elastic-system.svc.cluster.local:9200/'
```

A resolved name with a timeout points at a NetworkPolicy: list them with `kubectl -n
elastic-system get networkpolicy -o yaml` and admit the whole Thorium namespace, as shown in
[Converting a Pre-Helm Deployment](./convert-to-helm.md#before-you-start). The same holds for
policies guarding Scylla, Redis, or S3. A `401` from `curl` means the network path is fine.

## Rollouts

| Message | Cause and fix |
|---------|---------------|
| Waiting for `<deployments>` to roll out (phase `Provisioning`) | Normal while pods start. When a container fails, the message adds its reason, such as a crash loop, an image pull error, or a non-zero exit. Check `kubectl -n thorium describe pod` and the pod logs. A `Ready` cluster also shows this when a component becomes unavailable; `pod <name> is not ready although its containers are, so its node may be down or unreachable` or `pod <name> is not scheduled: ...` points at a down node (`kubectl get nodes`). Kubernetes replaces pods from a node that stays down after about 5 minutes, and the cluster goes back to `Ready` once the component is available again |
| Failed to provision nodes: node `<name>`: ... (phase `Error`) | The operator couldn't create a provision pod on that node. The components are still deployed and the other nodes provisioned; the failed node isn't labelled. Fix the cause in the message (for example pod permissions or a pod stuck with the provision pod's name) and the operator retries every minute |
| rollout exceeded its progress deadline: ... (phase `Error`) | A Deployment stopped making progress. Fix the cause (image, resources, config) and the next reconcile retries |
| Components rolled out but the bootstrap is incomplete: ... | A bootstrap step needs a Secret that is missing (named in the message). Create it; the bootstrap is retried |

A new image pushed under the same tag doesn't roll anything out; restart the components with
`kubectl -n thorium rollout restart deployment`. See [Rollouts](./helm-configuration.md#rollouts).
Nor does it run the schema apply again: set the `thorium.sandia.gov/retry-schema-apply`
annotation on the `ThoriumCluster` to a new value after the restart (see [Schema apply and
drift](./upgrade-paths.md#schema-apply-and-drift)).

## After an upgrade or conversion

Problems seen when converting and upgrading long-lived deployments are listed with their causes
and fixes in [Known issues and remediation](./upgrade-paths.md#known-issues-and-remediation). The
most common:

| Symptom | Where |
|---------|-------|
| The operator logs `401` for its `thorium-operator` user in a loop and never reaches `Ready` | [Logins](./upgrade-paths.md#logins) |
| A local account from an older release can't log in after the upgrade | [Logins](./upgrade-paths.md#logins) |
| `SchemaDrift` lists old tables, the upgrade system's tables, or leftovers from unreleased builds | [Schema apply and drift](./upgrade-paths.md#schema-apply-and-drift) |
| Ingested repos show no data, or view-backed listings miss rows, on an old keyspace | [Schema apply and drift](./upgrade-paths.md#schema-apply-and-drift) |
| A setting from `site.yaml` (such as a component's proxy `env`) is missing after the install | [Conversion and Helm](./upgrade-paths.md#conversion-and-helm) |
| External MCP clients lose their sessions, or a hand-made `/api/mcp` route is left over | [Conversion and Helm](./upgrade-paths.md#conversion-and-helm) |
| A backup exits 2 with skipped records, or thoradm can't find its Thorctl config | [Backups and restores](./upgrade-paths.md#backups-and-restores) |

## The toolbox import Job

`operator.toolbox` runs `thorctl toolbox import` in a Job named `toolbox-import-<hash>` (see
[Importing a toolbox](./deploy-helm.md#importing-a-toolbox)). Read what it did with
`kubectl -n thorium logs job/toolbox-import-<hash>`; it prints `thorctl`'s report and, on failure,
`The toolbox import failed (thorctl exit code N); see its report above`.

| Log line or symptom | Cause and fix |
|---------------------|---------------|
| `Waiting for the Thorium API and the admin user ... (N/120): <thorctl error>` | Normal while the API starts and the operator creates the admin. The Job gives up after about 20 minutes; the last `thorctl` error says which is missing. Check the API pod and the `thorium-admin` Secret |
| `Group X: not created, an existing one is left as is (...)` or `Network policy X: not created, ...` | Normal on a repeat run, since the group or policy already exists. Otherwise the message in brackets is the API's reason; the import then reports anything it still can't find |
| `tool NAME@LABEL: only an admin can publish a workload with a non-default security context` | The import logs in as the `operator.cluster.bootstrap.admin.secret` user. A non-default `security_context` (the repository's tools run as root) needs that user to be an admin |
| `group X doesn't exist; use --create-groups to create it` | Add the group to `operator.toolbox.groups`, or set `operator.toolbox.createGroups: true` |
| `you can't create tools, toolsets, agents, workflows, or pipelines in group X` | The admin's role or the group's settings don't allow writes there; fix the group or use `groupOverride` |
| Conflicts listed and nothing applied | With `mode: overwrite` conflicts are applied; with `skip-conflicts` existing items that differ are left alone and listed, which is not a failure |
| Conversion issues and `--strict` | `operator.toolbox.strict: true` fails the import when converting a legacy toolbox skipped or dropped something; fix the toolbox (see `thorctl toolbox convert`) or unset `strict` |
| The Job fails again and again | Each pod retry repeats the import, which only applies what is missing; the Job fails after `operator.toolbox.backoffLimit` (6) retries. Fix the cause shown in the log; changing any toolbox value creates a new Job |

## Helm render failures

| Message | Cause and fix |
|---------|---------------|
| `<old>` is not a chart value; use `<new>` (for example "operator.cluster.scaler.nodes is not a chart value; use operator.cluster.scaler.k8s.nodes") | A values file uses a key that moved. Use the key the message names |
| infra.`<svc>`.enabled is not a chart value; set global.managed.`<svc>` instead | Use the `global.managed` toggle |
| secrets.consumers is not a chart value: ... | Remove it; the consumer secrets follow `global.managed` and `global.quickwit.enabled` |
| Helm can't read the existing thorium-credentials secret here (helm template, helm lint, --dry-run without =server, or a GitOps renderer such as Argo CD or Flux), so these credentials would be regenerated: ... | Set every listed credential under `global.credentials` (from your backup or secret store), or set `secrets.renderOnly=true` for output that is never applied. See [Credentials](./helm-configuration.md#credentials) |
| This upgrade found no thorium-credentials secret in `<ns>`, so these credentials would be regenerated: ... | Restore `thorium-credentials` from your backup, or set every credential. Only set `secrets.allowRegenerate=true` for a deployment with no data to keep |
| secrets.renderOnly is only for rendering without a cluster (helm template, helm lint) and must not be set for helm install/upgrade, since it would store placeholder credentials | Remove `secrets.renderOnly` from a real install or upgrade |
| global.credentials.`<name>` is required when global.managed.`<svc>` is false, since the external service issues it | Set the external Redis password or S3 keys |
| global.manageSecretUpdates.`<svc>` can't be true: ... | Thorium can't change that service's password (an external Redis, S3 store, or Postgres, or an external Scylla without its bootstrap). Change the credential in the service yourself and leave the value `null` or `false`. See [Changing credentials](./helm-configuration.md#changing-credentials) |
| external Scylla without bootstrap.scylla needs global.credentials.scyllaUsername/scyllaPassword for the role you created (global.credentials.`<key>` is not set) | Set the role's credentials, or enable `operator.cluster.bootstrap.scylla` |
| external Elastic without bootstrap.elastic needs global.credentials.elasticUsername/elasticPassword for the user you created (global.credentials.`<key>` is not set) | Set the user's credentials, or set `operator.cluster.bootstrap.elastic.adminSecret.name` |
| operator.backends.`<svc>`... is required when global.managed.`<svc>` is false | Point the operator at the external service |
| global.quickwit.s3Endpoint is required with Quickwit when global.managed.s3 is false (and the quickwit bucket must already exist in that store) | Set it, or turn Quickwit off with `global.quickwit.enabled: false` |
| global.quickwit.metastoreUri is required with Quickwit when global.managed.postgres is false (and the metastore database must already exist) | Set it, or turn Quickwit off |
| operator.backends.elastic.caSecret is only for an external Elasticsearch; set global.managed.elastic to false or unset caSecret.name | The chart's Elasticsearch stays unverified |
| operator.ingress.registryHost routes to the infra subchart's registry, so it needs infra.registry.enabled | Enable the registry or unset `registryHost` |
| operator.imagePullSecrets must not list registry-token: ... | Remove `registry-token`; the operator manages it from `operator.cluster.registryAuth` |
| global.clusterCIDRs must be a list of CIDRs, e.g. [10.96.0.0/12, 10.244.0.0/16] | Use a YAML list |
| values don't meet the specifications of the schema(s): at '/global/credentials': additional properties '`<key>`' not allowed | `global.credentials` only takes the keys listed in its values; fix the misspelled key (the S3 secret is `s3SecretToken`) |
| values don't meet the specifications of the schema(s): at '/cluster/config/thorium/namespace': '`<value>`' does not match pattern '^[a-z][a-z0-9_]{0,47}$' | `operator.cluster.config.thorium.namespace` takes only lowercase letters, digits and underscores, starting with a letter, at most 48 characters. See [Namespaces](./helm-configuration.md#namespaces) |
| scylla-operator.webhook.tls needs caCrt, crt, and key set together (PEM) | Set all three for a fixed webhook certificate, or none (with `createSelfSignedCertificate: false`, set only `caCrt`). See [The infra-operators chart](./helm-configuration.md#the-infra-operators-chart) |
| ScyllaOperatorConfig "cluster" in namespace "" exists and cannot be imported into the current release: invalid ownership metadata | The Scylla operator created `cluster` before the infra-operators chart rendered it (the release gained `global.imageRegistry`); rerun the `helm upgrade` with `--take-ownership` |
| operator.cluster.components.`<name>` must be a map of settings, {} for the defaults, or null to omit it | Fix the component value |
| install the thorium chart into the thorium namespace or `<prefix>`-thorium, such as dev-thorium (got `"<ns>"`) | Install into `thorium` or `<prefix>-thorium` |
| namespace `<ns>` already holds ThoriumCluster `<name>`, which Helm doesn't manage. A deployment from the pre-Helm minithor or megathor scripts must be converted ... | Convert it first: [Converting a Pre-Helm Deployment](./convert-to-helm.md) |
| namespace `<ns>` already holds ThoriumCluster `<name>` from Helm release `<release>`; only one Thorium release may use a namespace | Upgrade that release instead of installing another |
| namespace `<ns>` already holds ThoriumCluster `<name>` from this release, but operator.cluster.name is `<other>`; set operator.cluster.name to `<name>` ... | `operator.cluster.name` is fixed after the first install; set it back |
| ThoriumCluster `<ns>/<name>` already exists. Thorium supports one deployment per Kubernetes cluster, and namespace prefixes only rename namespaces; uninstall that deployment before installing this one in namespace `<ns>` | Another deployment owns this Kubernetes cluster |

## Helm install conflicts

Helm 4 applies charts server-side, so it fails when another field manager owns a field the chart
sets. Helm 3 creates and patches objects client-side without checking field ownership.

| Message | Cause and fix |
|---------|---------------|
| failed to install CRD crds/thoriumcluster.yaml: conflict occurred while applying object /thoriumclusters.sandia.gov ... conflicts with "thorium_cluster_apply": .spec.names.categories, .spec.names.shortNames | The `ThoriumCluster` CRD already exists, applied by the operator (field manager `thorium_cluster_apply`) or by `convert-to-helm.sh`, and Helm 4 installs a chart's `crds/` with server-side apply. This happens on the first install after a conversion, or on an install over a cluster that kept the CRD (`helm uninstall` leaves it). Run the install again with `--skip-crds`, which leaves the existing CRD alone as Helm 3 does; the operator applies its own CRD when it starts, and the chart's pre-upgrade hook applies it on every upgrade. minithor and megathor pass it whenever the CRD exists |
| conflict occurred while applying object thorium/banner /v1, Kind=ConfigMap: Apply failed with 1 conflict: conflict with "kubectl-create" using v1: .data.banner.txt (or `"before-first-apply"`, or ServiceAccount `thorium` or a namespace) | The object was created by the pre-Helm scripts and handed to the release by a `convert-to-helm.sh` that left its field ownership with its creator (`kubectl-create`), and the chart sets one of its fields to another value (that script dropped the banner's trailing newline). Clearing `managedFields` to `[{}]` doesn't help: Helm's next apply gives every field to `before-first-apply`. Hand the fields to the release with the patch below for each object an error names, then run the install again; Helm then writes the chart's values, so check that `operator.banner` in the converted values holds the banner you want. For the `banner` ConfigMap, deleting it (`kubectl -n thorium delete configmap banner`) and running the install again also works, since the chart then creates it |
| conflict occurred while applying object `<ns>/<name>` ... ThoriumCluster: Apply failed with ... conflict with "kubectl-edit" (or "kubectl-patch") | Someone changed the Helm-owned `ThoriumCluster` with `kubectl edit` or `kubectl patch`, so that field manager owns the fields. Put the change in your values, remove the manager's `managedFields` entry, and run the upgrade again (see [Conversion and Helm](./upgrade-paths.md#conversion-and-helm)) |

```bash
# the object the error names, such as configmap banner -n thorium or serviceaccount thorium -n thorium
kubectl patch configmap banner -n thorium --type merge -p '{"metadata":{"managedFields":[{
  "manager":"convert-to-helm","operation":"Update","apiVersion":"v1","fieldsType":"FieldsV1",
  "fieldsV1":{"f:metadata":{"f:annotations":{"f:meta.helm.sh/release-name":{},
  "f:meta.helm.sh/release-namespace":{}},"f:labels":{"f:app.kubernetes.io/managed-by":{}}}}}]}}'
```

The patch leaves one entry owning only the Helm release annotations and `managed-by` label the
conversion set (they must be on the object, as they are on every object the conversion handed
over), so no other field manager owns a field the chart sets.

## Uninstalling

| Symptom | Cause and fix |
|---------|---------------|
| `helm uninstall` fails after "Timed out after `<n>`s waiting for the operator to clean up the ThoriumCluster" in the hook Job's log | The operator isn't running or can't finish its cleanup. Fix the operator, or uninstall with `--no-hooks` (see [Uninstalling](./operate.md#uninstalling)). Keep `operator.uninstallHook.timeoutSeconds` below Helm's `--timeout` |
| The `ThoriumCluster` stays `Terminating` | No operator is running to remove its finalizer, or the operator's cleanup keeps failing: it retries every minute and puts the reason in the status message (`kubectl -n thorium get thoriumcluster <name> -o jsonpath='{.status.message}'`) and the operator log, such as a forbidden error when it can no longer list or patch nodes to remove their labels. Start the operator again or fix the cause, or as a last resort `kubectl -n thorium patch thoriumcluster <name> --type merge -p '{"metadata":{"finalizers":null}}'` and clean up node labels and provision pods yourself |

## Agents are outdated or jobs stay `Created`

Agents exit without claiming jobs when their version doesn't match the API's. The operator
replaces provision pods (and so the agents) once the API reports a new version: the node watcher
checks each node every 15 minutes, and every `ThoriumCluster` reconcile checks too. To reinstall
them now, for example after pushing a new image under the same tag, delete them and restart the
operator: nothing watches provision pods, so a deleted one only comes back on the next reconcile,
which the operator runs for its `ThoriumCluster` when it starts:

```bash
kubectl -n thorium delete pod -l app=node-provisioner
kubectl -n thorium rollout restart deployment/operator
```

A node that is down isn't provisioned until it is ready again. It keeps its `thorium=enabled`
label and stays registered in Thorium (the operator never unregisters nodes), which is harmless
since Kubernetes doesn't schedule onto a `NotReady` node. When it comes back, the node watcher
leaves the already labelled node alone, and the next reconcile recreates its provision pod if
Kubernetes deleted it meanwhile (restart the operator, as above, to do that at once). See
[Nodes](./thoriumcluster.md#nodes).

Node provisioning pauses while a cluster is in `UpgradeRequired` or `Upgrading`. See also [Jobs
Stuck At Created](../common_issues/jobs_stuck_at_created.md).
