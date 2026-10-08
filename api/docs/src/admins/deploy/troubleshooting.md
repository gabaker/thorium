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

### Invalid or impossible targets

| Message | Cause and fix |
|---------|---------------|
| Revision `"<raw>"` is not a revision id like 2026-10-v02 | The target isn't `YYYY-MM-vNN`. Fix the value |
| Revision `<rev>` is not known to this Thorium build (latest is `<latest>`) | The target is newer than this operator, or a revision that doesn't exist. Use a revision from `thoradm migrate list` of the same build |
| The target revision `<target>` is older than this cluster's revision `<current>`; upgrades never move backwards | Set the target to the current or a newer revision |
| This cluster is at revision `<current>`, which is newer than this Thorium build's latest revision `<latest>`; deploy a Thorium build that knows revision `<current>` (downgrades are not supported) | The operator was rolled back. Deploy the newer operator again |
| This cluster's upgrade state (schema version `<n>`) was written by a newer Thorium operator than this one ... | The `thorium-upgrade-state` ConfigMap was written by a newer operator. Deploy that operator again |

These hold the cluster in `Error` and are checked again every hour or when the spec changes.

### "is blocked at step elastic-reindex-keyword-mappings"

> The upgrade to 2026-10-v02 is blocked at step elastic-reindex-keyword-mappings: Elastic indexes
> `<indexes>` do not map group as a keyword and must be reindexed. Waiting for an approval:
> ...

The step found work to do and needs an approval naming your backup. Add
`{step: elastic-reindex-keyword-mappings, backup: <where your backup is>}` to
`spec.upgrade.approvals` (Helm `operator.cluster.upgrade.approvals`). An approval whose `backup` is
empty doesn't count, and the message says so ("names no backup"). See
[Approvals](./upgrades.md#approvals).

> ... Waiting for manual work: step elastic-reindex-keyword-mappings is approved, but the operator
> can't run it on its own, so follow its procedure: ...

The step is approved but the operator doesn't reindex by itself. Follow [Reindexing
Elasticsearch](./upgrades.md#reindexing-elasticsearch); the operator checks every minute and
records the step as `Done` ("fixed manually: ...") once the indexes are correct. minithor and
megathor stop waiting with an error as soon as a step is `Blocked`.

## Config errors

| Message | Cause and fix |
|---------|---------------|
| config secret `<name>` not found in `<ns>` | A `config_secrets` entry names a Secret that doesn't exist in the cluster's namespace |
| config secret `<name>` in `<ns>` has no key `<key>` | The Secret exists but lacks the key (`thorium.yml` by default) |
| bootstrap admin secret `<name>` not found in `<ns>` | A bootstrap Secret is missing |
| Secret `<name>` in `<field>` collides with a Secret the operator manages; rename it (reserved: thorium, keys, keys-kaboom, docker-skopeo, registry-token, thorium-image-pull, and any name ending in -pass) | Rename your Secret |
| Config secret `<ns>/<name>` is not valid YAML: ... | Fix the document in the Secret |
| Invalid Thorium config from spec.config merged with config secrets [...]: ... | The merged config doesn't match Thorium's config types: a wrong type, a quoted number, or a YAML tag such as `!Grpc`. See [Config merging](./thoriumcluster.md#config-merging) |

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
| skip_bucket_auto_create is set but `<n>` required S3 bucket(s) are missing or inaccessible at `<endpoint>`: ... | Create the listed buckets or grant Thorium's S3 identity access to them |
| Waiting for Redis at `<addr>` to accept connections: ... | Redis isn't up yet, or `redis.host`/`port` is wrong |
| Elastic index `<name>` does not map group as a keyword, so group filters may not ... (alongside `Ready`) | The index was created from dynamic mappings. Reindex it: see [Reindexing Elasticsearch](./upgrades.md#reindexing-elasticsearch) |

When the operator reaches the Kubernetes cluster through a context its config doesn't list, the
operator log shows:

> The operator reaches this Kubernetes cluster through kube context '`<context>`', but
> thorium.scaler.k8s.clusters only has config for [...], so its nodes are not provisioned or
> labelled for Thorium. ...

Key the cluster by that context (Helm value `operator.cluster.scaler.k8s.context`). An operator
running in the cluster always uses `kubernetes-admin@cluster.local`.

## Rollouts

| Message | Cause and fix |
|---------|---------------|
| Waiting for `<deployments>` to roll out (phase `Provisioning`) | Normal while pods start. When a container fails, the message adds its reason, such as a crash loop, an image pull error, or a non-zero exit. Check `kubectl -n thorium describe pod` and the pod logs. A `Ready` cluster also shows this when a component becomes unavailable; `pod <name> is not ready although its containers are, so its node may be down or unreachable` or `pod <name> is not scheduled: ...` points at a down node (`kubectl get nodes`). Kubernetes replaces pods from a node that stays down after about 5 minutes, and the cluster goes back to `Ready` once the component is available again |
| Failed to provision nodes: node `<name>`: ... (phase `Error`) | The operator couldn't create a provision pod on that node. The components are still deployed and the other nodes provisioned; the failed node isn't labelled. Fix the cause in the message (for example pod permissions or a pod stuck with the provision pod's name) and the operator retries every minute |
| rollout exceeded its progress deadline: ... (phase `Error`) | A Deployment stopped making progress. Fix the cause (image, resources, config) and the next reconcile retries |
| Components rolled out but the bootstrap is incomplete: ... | A bootstrap step needs a Secret that is missing (named in the message). Create it; the bootstrap is retried |

A new image pushed under the same tag doesn't roll anything out; restart the components with
`kubectl -n thorium rollout restart deployment`. See [Rollouts](./helm-configuration.md#rollouts).

## Helm render failures

| Message | Cause and fix |
|---------|---------------|
| `<old>` is not a chart value; use `<new>` (for example "operator.cluster.scaler.nodes is not a chart value; use operator.cluster.scaler.k8s.nodes") | A values file uses a key that moved. Use the key the message names |
| infra.`<svc>`.enabled is not a chart value; set global.managed.`<svc>` instead | Use the `global.managed` toggle |
| secrets.consumers is not a chart value: ... | Remove it; the consumer secrets follow `global.managed` and `global.quickwit.enabled` |
| Helm can't read the existing thorium-credentials secret here (helm template, helm lint, --dry-run without =server, or a GitOps renderer such as Argo CD or Flux), so these credentials would be regenerated: ... | Set every listed credential under `secrets.credentials` (from your backup or secret store), or set `secrets.renderOnly=true` for output that is never applied. See [Credentials](./helm-configuration.md#credentials) |
| This upgrade found no thorium-credentials secret in `<ns>`, so these credentials would be regenerated: ... | Restore `thorium-credentials` from your backup, or set every credential. Only set `secrets.allowRegenerate=true` for a deployment with no data to keep |
| secrets.renderOnly is only for rendering without a cluster (helm template, helm lint) and must not be set for helm install/upgrade, since it would store placeholder credentials | Remove `secrets.renderOnly` from a real install or upgrade |
| secrets.credentials.`<name>` is required when global.managed.`<svc>` is false, since the external service issues it | Set the external Redis password or S3 keys |
| external Scylla without bootstrap.scylla needs secrets.credentials.scyllaUsername/scyllaPassword for the role you created (secrets.credentials.`<key>` is not set) | Set the role's credentials, or enable `operator.cluster.bootstrap.scylla` |
| external Elastic without bootstrap.elastic needs secrets.credentials.elasticUsername/elasticPassword for the user you created (secrets.credentials.`<key>` is not set) | Set the user's credentials, or set `operator.cluster.bootstrap.elastic.adminSecret.name` |
| operator.backends.`<svc>`... is required when global.managed.`<svc>` is false | Point the operator at the external service |
| global.quickwit.s3Endpoint is required with Quickwit when global.managed.s3 is false (and the quickwit bucket must already exist in that store) | Set it, or turn Quickwit off with `global.quickwit.enabled: false` |
| global.quickwit.metastoreUri is required with Quickwit when global.managed.postgres is false (and the metastore database must already exist) | Set it, or turn Quickwit off |
| operator.backends.elastic.caSecret is only for an external Elasticsearch; set global.managed.elastic to false or unset caSecret.name | The chart's Elasticsearch stays unverified |
| operator.ingress.registryHost routes to the infra subchart's registry, so it needs infra.registry.enabled | Enable the registry or unset `registryHost` |
| operator.imagePullSecrets must not list registry-token: ... | Remove `registry-token`; the operator manages it from `operator.cluster.registryAuth` |
| global.clusterCIDRs must be a list of CIDRs, e.g. [10.96.0.0/12, 10.244.0.0/16] | Use a YAML list |
| values don't meet the specifications of the schema(s): secrets: at '/credentials': additional properties '`<key>`' not allowed | `secrets.credentials` only takes the keys listed in its values; fix the misspelled key (the S3 secret is `s3SecretToken`) |
| scylla-operator.webhook.tls needs caCrt, crt, and key set together (PEM) | Set all three for a fixed webhook certificate, or none (with `createSelfSignedCertificate: false`, set only `caCrt`). See [The infra-operators chart](./helm-configuration.md#the-infra-operators-chart) |
| ScyllaOperatorConfig "cluster" in namespace "" exists and cannot be imported into the current release: invalid ownership metadata | The Scylla operator created `cluster` before the infra-operators chart rendered it (the release gained `global.imageRegistry`); rerun the `helm upgrade` with `--take-ownership` |
| operator.cluster.components.`<name>` must be a map of settings, {} for the defaults, or null to omit it | Fix the component value |
| install the thorium chart into the thorium namespace or `<prefix>`-thorium, such as dev-thorium (got `"<ns>"`) | Install into `thorium` or `<prefix>-thorium` |
| namespace `<ns>` already holds ThoriumCluster `<name>`, which Helm doesn't manage. A deployment from the pre-Helm minithor or megathor scripts must be converted ... | Convert it first: [Converting a Pre-Helm Deployment](./convert-to-helm.md) |
| namespace `<ns>` already holds ThoriumCluster `<name>` from Helm release `<release>`; only one Thorium release may use a namespace | Upgrade that release instead of installing another |
| namespace `<ns>` already holds ThoriumCluster `<name>` from this release, but operator.cluster.name is `<other>`; set operator.cluster.name to `<name>` ... | `operator.cluster.name` is fixed after the first install; set it back |
| ThoriumCluster `<ns>/<name>` already exists. Thorium supports one deployment per Kubernetes cluster, and namespace prefixes only rename namespaces; uninstall that deployment before installing this one in namespace `<ns>` | Another deployment owns this Kubernetes cluster |

## Uninstalling

| Symptom | Cause and fix |
|---------|---------------|
| `helm uninstall` fails after "Timed out after `<n>`s waiting for the operator to clean up the ThoriumCluster" in the hook Job's log | The operator isn't running or can't finish its cleanup. Fix the operator, or uninstall with `--no-hooks` (see [Uninstalling](./operate.md#uninstalling)). Keep `operator.uninstallHook.timeoutSeconds` below Helm's `--timeout` |
| The `ThoriumCluster` stays `Terminating` | No operator is running to remove its finalizer. Start the operator again, or as a last resort `kubectl -n thorium patch thoriumcluster <name> --type merge -p '{"metadata":{"finalizers":null}}'` and clean up node labels and provision pods yourself |

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
