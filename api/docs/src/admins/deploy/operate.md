# Operating a Deployment

This page covers day-to-day work on a running Kubernetes deployment of Thorium: reading its
status, finding its Secrets and config, reaching the backing services, changing configuration,
scaling, backups, and uninstalling. The examples use the thorium namespace `thorium` and the
`ThoriumCluster` name `thorium`; substitute `<prefix>-thorium` and your cluster's name where they
differ.

## Status

```bash
kubectl -n thorium get thoriumcluster -o wide
```

The `Phase` and `Revision` columns show the phase and the revision the deployment is at, and
`-o wide` adds the `Message` column. See
[the phases](./concepts.md#the-operator-and-the-thoriumcluster-resource) and
[ThoriumCluster and Operator Reference](./thoriumcluster.md#status) for the full status. To wait
for a change to finish:

```bash
kubectl -n thorium wait thoriumcluster/thorium --for=jsonpath='{.status.phase}'=Ready --timeout=30m
```

`status.observed_generation` names the spec generation the status describes, so compare it with
`metadata.generation` to tell whether the operator has seen your latest change. The operator
rechecks a deployment that is waiting on a backend or rollout every 15 seconds, and a `Ready`
deployment once a day or whenever its spec, a Secret it names, the `banner` ConfigMap, or the
availability of a component Deployment changes. A component that becomes unavailable, for example
when its node goes down, moves the deployment to `Provisioning` until it recovers, without
rolling anything out.
The operator's log has the details:

```bash
kubectl -n thorium logs deploy/operator
```

## Where things are

| Object | Holds |
|--------|-------|
| Secret `thorium` (key `thorium.yml`) | The rendered Thorium config every component mounts, credentials included |
| Secret `keys` | The API keys the scaler, event handler, and search streamer use |
| Secret `thorium-operator-pass` | The operator's own Thorium password; kept when the `ThoriumCluster` is deleted |
| Secret `thorium-credentials` | Every credential the chart generated or was given (Helm deployments) |
| Secret `thorium-config-secrets` | The credentials as a partial `thorium.yml`, merged by the operator (Helm deployments) |
| Secret `thorium-admin` | The initial admin user's username and password (Helm deployments) |
| ConfigMap `banner` | The login banner the UI shows |
| ConfigMap `tracing-conf` | The components' tracing config |
| ConfigMap `thorium-upgrade-state` | The revision the deployment is at and every upgrade step applied (see [Upgrading Thorium](./upgrades.md)) |

Write the rendered config to a file, for example for [thoradm](../thoradm/thoradm.md):

```bash
kubectl -n thorium get secret thorium -o go-template='{{index .data "thorium.yml" | base64decode}}' > thorium.yml
chmod 600 thorium.yml
```

## Reaching the services

The services keep their in-cluster names; forward the ones you need to your workstation:

```bash
# the Thorium API and UI at http://localhost:8080
kubectl -n thorium port-forward svc/thorium-api 8080:80
# Kibana at https://localhost:5601 (log in as the elastic superuser below)
kubectl -n elastic port-forward svc/elastic-kb-http 5601
# Elasticsearch at https://localhost:9200
kubectl -n elastic port-forward svc/elastic-es-http 9200
# the Jaeger trace viewer at http://localhost:16686 (with global.quickwit.enabled)
kubectl -n jaeger port-forward svc/jaeger 16686
```

The chart's Elasticsearch superuser password (ECK's `elastic` user):

```bash
kubectl -n elastic get secret elastic-es-elastic-user -o jsonpath='{.data.elastic}' | base64 -d; echo
```

[thoradm](../thoradm/thoradm.md) can run inside the cluster, where the config's service names
resolve:

```bash
kubectl -n thorium exec deploy/api -- /app/thoradm --help
```

## Changing configuration

Change Helm deployments with `helm upgrade` and your values files. `--reuse-values` reuses the
values of the last release, including any `--set` given then; passing every values file each
time is easier to reason about:

```bash
helm upgrade thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION \
  -n thorium -f site.yaml
```

The operator rolls out a component only when its config, a mounted input, or its own spec
changes (see [Rollouts](./helm-configuration.md#rollouts)), so most changes restart only what
they affect. A new image pushed under the same tag needs a manual restart:

```bash
kubectl -n thorium rollout restart deployment
```

Node provision pods, which install the agent on each worker node, are replaced once the API
reports a new Thorium version (the node watcher checks each node every 15 minutes) or on the next
reconcile. Nothing watches the provision pods themselves, so a deleted one comes back only on the
next `ThoriumCluster` reconcile, which for a `Ready` deployment may be a day away. To reinstall the
agent at once, delete the provision pods and restart the operator, which reconciles the
`ThoriumCluster` when it starts:

```bash
kubectl -n thorium delete pod -l app=node-provisioner
kubectl -n thorium rollout restart deployment/operator
```

## Scaling and nodes

- **Worker nodes**: `operator.cluster.scaler.k8s.nodes` lists the nodes Thorium may run jobs on
  (empty means every node). The operator provisions the agent on them, labels them, and
  registers them with Thorium after deploying the components, so a node that fails to provision
  only marks the deployment `Error` (retried every minute) without holding back the components.
  A node that goes down keeps its `thorium=enabled` label and its registration, which is harmless
  since Kubernetes doesn't schedule onto a `NotReady` node. When it returns, the node watcher sees
  it already labelled and leaves it alone; the next `ThoriumCluster` reconcile recreates its
  provision pod if Kubernetes deleted it meanwhile (restart the operator to reconcile at once, as
  above). See [Nodes](./thoriumcluster.md#nodes).
- **API replicas**: `operator.cluster.components.api.replicas`.
- **Component resources**: `operator.cluster.components.<component>.resources` (see
  [Components](./helm-configuration.md#components)).
- **Backing services**: `infra.scylla.members`, `infra.elastic.count`, and `infra.postgres.replicas`
  scale the chart's databases through their operators. Keep
  `operator.backends.scylla.replication` at most the number of Scylla nodes; raising it later
  doesn't change the replication of the existing keyspace.

## Backups

Back up before every upgrade and before converting a deployment:

- **Thorium's data**: `thoradm backup new` backs up Redis, Scylla, and S3 (files, repos,
  results, and comment attachments); `thoradm backup restore` restores it and
  `thoradm backup scrub` checks a backup for corruption. See
  [Admin Command Line Tool](../thoradm/thoradm.md#backup).
- **Elasticsearch** holds only derived data: thoradm doesn't back it up, and the search streamer
  rebuilds it from Scylla (see
  [Reindexing Elasticsearch](./upgrades.md#reindexing-elasticsearch)). Take Elasticsearch
  snapshots only if you want a faster restore than a reindex.
- **Credentials and settings**: the `thorium-credentials` Secret (Helm deployments; without it
  every user is locked out after a restore onto a new install), your values files, and for
  deployments without Helm the `ThoriumCluster` and its config Secrets.

```bash
kubectl -n thorium get secret thorium-credentials -o yaml > thorium-credentials-backup.yaml
kubectl -n thorium get thoriumcluster thorium -o yaml > thoriumcluster-backup.yaml
```

Both files hold credentials: store them as securely as the backups themselves.

## Uninstalling

```bash
helm uninstall thorium -n thorium
```

A `pre-delete` hook (`operator.uninstallHook`) deletes the `ThoriumCluster` first and waits for
the still-running operator to clean up: node labels, node provision pods, the component
Deployments, the `thorium-api` and `thorium-mcp` Services, and the Secrets and ConfigMaps it
created. Helm waits up to `--timeout` (5 minutes by default) for the hook, so keep
`operator.uninstallHook.timeoutSeconds` (default 240) below it or pass a larger `--timeout`.

If the operator isn't running, the hook times out. Skip it with `--no-hooks`; Helm then deletes
the `ThoriumCluster` with the rest of the release, and with no operator to remove its finalizer it
stays `Terminating`. As a last resort, remove the finalizer by hand, then remove the node labels
and provision pods yourself:

```bash
helm uninstall thorium -n thorium --no-hooks
kubectl -n thorium patch thoriumcluster thorium --type merge -p '{"metadata":{"finalizers":null}}'
```

`operator.uninstallHook.enabled=false` leaves the hook out of the release on later installs and
upgrades; it doesn't affect uninstalling a release that already has it.

**What is kept:** the namespaces, the volumes (and so the data), the `thorium-credentials`,
`thorium-config-secrets`, and `thorium-admin` Secrets, the `thorium-operator-pass` Secret, the
`thorium-upgrade-state` ConfigMap, and the group namespaces the k8s scaler created. Installing the
chart again into the same namespace picks the data, credentials, and revision up again. If you
delete the databases but keep the namespace, also delete `thorium-operator-pass`, or the operator
can't log in to the new, empty deployment.

To remove the data, delete the namespaces (prefix each with `<prefix>-` where you used one), and
the group namespaces the scaler created and labelled:

```bash
kubectl delete namespace thorium redis scylla elastic seaweedfs quickwit jaeger
kubectl delete namespace -l app.kubernetes.io/managed-by=thorium-scaler
```

Group namespaces created by a scaler that didn't label them must be deleted by name.

Remove the operators after Thorium. Helm leaves the CRDs from the charts' `crds/` directories
(Scylla, Kubegres) and the ECK CRDs (which carry `helm.sh/resource-policy: keep`) installed, along
with the `ThoriumCluster` CRD the operator applies. Deleting a CRD deletes every resource of that
kind in the cluster:

```bash
helm uninstall infra-operators -n infra-operators
kubectl delete crd -l app.kubernetes.io/instance=infra-operators
kubectl get crd -o name | grep -E '\.scylla\.scylladb\.com$|/kubegres\.kubegres\.reactive-tech\.io$|/thoriumclusters\.sandia\.gov$' | xargs -r kubectl delete
```
