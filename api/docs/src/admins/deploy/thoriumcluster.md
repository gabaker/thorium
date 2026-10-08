# ThoriumCluster and Operator Reference

This page lists every field of the `ThoriumCluster` custom resource, what the Thorium operator
reports in its status, the names and labels it manages, its command line, and the permissions it
needs. Use it when writing a `ThoriumCluster` by hand ([Deploy the Operator Without
Helm](./deploy-thorium.md)), when reading the status of a running deployment ([Operating a
Deployment](./operate.md#status)), or when checking what the Helm chart renders for you
([Configure the Helm Charts](./helm-configuration.md)).

```bash
# the phase and revision of every ThoriumCluster, with the status message
kubectl -n thorium get thoriumcluster -o wide
# print the CRD schema the operator applies
kubectl explain thoriumcluster.spec --recursive
```

The resource is `thoriumclusters.sandia.gov`, version `v1`, kind `ThoriumCluster`, and it is
namespaced. The operator applies its own CRD when it starts (see [Operator command
line](#operator-command-line)).

## Spec

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `registry` | string | required | The Thorium image without its tag, such as `ghcr.io/cisagov/thorium/infrastructure/thorium`. Every component runs `<registry>:<version>` |
| `version` | string | `latest` | The image tag every component runs. The chart sets it to the chart's `appVersion` unless `operator.cluster.version` or `operator.image.tag` is set |
| `image_pull_policy` | string | `Always` | The pull policy of every component and node provision pod |
| `image_pull_secrets` | list of strings | `[]` | Existing pull secrets in the namespace that every Thorium pod references. The chart lists `thorium-image-pull` here when it renders one |
| `registry_auth` | map of registry host to base64 `user:password` | unset | Credentials for tool image registries. The operator writes them to the `registry-token` pull secret (which every Thorium pod then references) and to the scaler's `docker-skopeo` Secret |
| `components` | object | required | The components to deploy; see [Components](#components) |
| `config` | object | required | The non-secret part of `thorium.yml`. Any field may be left out and supplied by `config_secrets` instead |
| `config_secrets` | list of `{name, key}` | `[]` | Secrets in the namespace holding partial `thorium.yml` documents, merged over `config` in order. `key` defaults to `thorium.yml`; see [Config merging](#config-merging) |
| `bootstrap` | object | unset | Privileged setup the operator performs; see [Bootstrap](#bootstrap) |
| `elastic_ca_secret` | `{name, key}` | unset | A Secret holding the PEM CA that signed an external Elasticsearch's certificate. `key` defaults to `ca.crt`. See [Elasticsearch CA](#elasticsearch-ca) |
| `upgrade` | object | `{}` | How the operator moves the cluster between revisions; see [Upgrade](#upgrade) |

### Components

`components.api` is always deployed. `scaler`, `baremetal_scaler`, `search_streamer`, and
`event_handler` are deployed only when present (an empty object `{}` deploys one with its
defaults). Each takes the fields below; any field left out gets its default.

| Field | Type | Notes |
|-------|------|-------|
| `env` | list of `{name, value}` | Environment variables. Setting `env` replaces the whole default list, so include any default you still need |
| `cmd` | list of strings | The container command (the image's entrypoint is replaced) |
| `args` | list of strings | The container arguments. Setting `args` replaces the whole default list |
| `resources` | `{cpu, memory}` | CPU in millicpus (1000 is one core) and memory in MiB, applied as both the request and the limit. `0` leaves the container unbounded |
| `replicas` | integer | API only |
| `service_account` | boolean | Scaler only. `true` runs the scaler as the `thorium` service account; `false` (the CRD default) mounts the `kube-config` Secret (key `config`) at `/home/thorium/.kube/config` instead. A kube config ignores `no_proxy`, so a scaler with a proxy in its `env` also needs `thorium.scaler.k8s.clear_proxy: true` to reach the Kubernetes API directly. The chart sets `true` |

| Component | Deployment | Default `cmd` | Default `args` | Default `resources` |
|-----------|------------|---------------|----------------|---------------------|
| `api` | `api` | `/app/thorium-api` | `--config /conf/thorium.yml` | 2000 / 8192, 3 replicas |
| `scaler` | `scaler` | `/app/thorium-scaler` | `--config /conf/thorium.yml --auth /keys/keys.yml` | 2000 / 4096 |
| `baremetal_scaler` | `baremetal-scaler` | `/app/thorium-scaler` | `--config /conf/thorium.yml --auth /keys/keys.yml --scaler bare-metal` | 2000 / 4096 |
| `search_streamer` | `search-streamer` | `/app/thorium-search-streamer` | `--config /conf/thorium.yml --keys /keys/keys.yml` | 2000 / 4096 |
| `event_handler` | `event-handler` | `/app/thorium-event-handler` | `--config /conf/thorium.yml --auth /keys/keys.yml` | 2000 / 4096 |

The chart's defaults differ from the CRD's: one API replica and `resources: {cpu: 0, memory: 0}`
(unbounded) for every component.

The default `env` of every component sets `http_proxy`, `https_proxy`, `HTTP_PROXY`, and
`HTTPS_PROXY` to empty values and `no_proxy`/`NO_PROXY` to `localhost,cluster.local`. The operator
sets a scaler's `HOME` (where it reads registry credentials), and its `KUBECONFIG` when it uses
the `kube-config` Secret, over any value in `env`. To give a component a proxy, set its whole `env`:

```yaml
components:
  scaler:
    service_account: true
    env:
      - { name: https_proxy, value: http://proxy.example.com:3128 }
      - { name: HTTPS_PROXY, value: http://proxy.example.com:3128 }
      - { name: no_proxy, value: "localhost,127.0.0.1,.svc,.svc.cluster.local,cluster.local" }
      - { name: NO_PROXY, value: "localhost,127.0.0.1,.svc,.svc.cluster.local,cluster.local" }
```

### Config merging

Each `config_secrets` document is parsed as plain YAML and merged over `config` as a JSON merge
patch (RFC 7386): objects merge key by key, lists and scalars replace the earlier value, and a
`null` deletes the key. The merged result must deserialize into Thorium's config, which is
stricter than loading a `thorium.yml` file:

- YAML tags (such as `!Grpc`), non-string map keys, and merge keys (`<<`) are not supported.
  Write enum values in map form, for example `external: {Grpc: {endpoint: ..., level: Info}}`.
- Values are not coerced: a quoted `"5"` stays a string, so every value must already have the
  type Thorium expects.

The rendered `thorium.yml` is written to the `thorium` Secret (key `thorium.yml`) only after every
backend check passes.

### Bootstrap

Every Secret named here is read from the `ThoriumCluster`'s own namespace. A Secret reference
(`SecretCredentials`) has these fields:

| Field | Notes |
|-------|-------|
| `name` | The Secret's name |
| `username` | A literal username |
| `username_key` | The key holding the username (wins over `username`) |
| `password_key` | The key holding the password (required) |

| Field | Notes |
|-------|-------|
| `bootstrap.admin.secret` | Create the initial Thorium admin user from this Secret, once, when the user doesn't exist yet |
| `bootstrap.scylla.admin_secret` | Scylla superuser credentials used to create Thorium's role (`scylla.auth` in the config). Without it the operator tries the default `cassandra`/`cassandra` superuser, which only exists on a fresh Scylla |
| `bootstrap.scylla.drop_default_role` | Drop the default `cassandra` role once Thorium's role exists (default `false`; the chart sets `true`) |
| `bootstrap.elastic.admin_secret` | Elasticsearch superuser credentials used to create Thorium's role and user (`elastic.username`) in an external Elasticsearch. Required when `bootstrap.elastic` is set |

The privileged steps run only when `bootstrap`, the rendered config, or a Secret they name
changes; `status.bootstrap_hash` records the inputs of the last completed run. Each step reads its
Secret only when it has work to do, so the Secrets may be deleted after setup. When a step needs a
deleted Secret, the components still roll out (except without a Scylla role, which they can't run
without) and the cluster reports `Error` naming the missing Secret.

### Elasticsearch CA

When `elastic_ca_secret` is set, the operator mounts the CA read-only at
`/etc/thorium/elastic-ca/ca.crt` in every component and validates Elasticsearch's certificate chain
and hostname against it, whatever the config's `cert_validation` and `insecure_certificates` say.
The operator checks Elasticsearch itself, so its own pod must mount the same Secret at the same
path; the chart does this, and a hand-written operator Deployment must too
([Deploy the Operator Without Helm](./deploy-thorium.md)). Rotating the CA rolls the components
out.

### Upgrade

| Field | Type | Notes |
|-------|------|-------|
| `upgrade.target_revision` | string matching `^[0-9]{4}-[0-9]{2}-v[0-9]{2}$` | The revision to upgrade to. Unset, a cluster behind the operator waits in `UpgradeRequired` |
| `upgrade.auto_target_dev` | boolean, default `false` | Target the operator's latest revision whenever `target_revision` is unset; meant for development clusters |
| `upgrade.approvals` | list of `{step, backup}` | Approvals for `Data` and `Manual` steps that have work to do. Only an approval with a non-empty `backup` counts. The operator never takes backups; `backup` is your reference to one you took |

See [Upgrading Thorium](./upgrades.md).

## Status

| Field | Notes |
|-------|-------|
| `phase` | `Provisioning`, `Ready`, `Error`, `UpgradeRequired`, or `Upgrading` |
| `message` | What the phase is waiting on or what failed. Non-fatal problems (such as an index whose `group` field isn't a keyword) are noted here alongside `Ready` |
| `last_transition` | When the phase or message last changed (RFC 3339) |
| `observed_generation` | The `metadata.generation` this status describes. A status whose `observed_generation` is older than `metadata.generation` hasn't caught up with the latest spec |
| `bootstrap_hash` | The inputs of the last completed bootstrap |
| `upgrade.current` | The revision the cluster is at |
| `upgrade.target` | The revision it is upgrading to |
| `upgrade.latest` | The newest revision this operator knows |
| `upgrade.steps` | The steps left to run, each with `revision`, `step`, `kind`, `description`, `quiesce`, `state` (`Pending`, `Running`, `Done`, `Skipped`, `Blocked`, `Failed`), and `message` |

`kubectl get thoriumcluster` prints the `Phase` and `Revision` (`status.upgrade.current`) columns;
`-o wide` adds `Message`.

| Phase | Meaning |
|-------|---------|
| `Provisioning` | Deploying, or waiting on something that finishes on its own: a backend that isn't accepting connections yet, Deployments still rolling out, or a component Deployment that became unavailable (for example because its node went down) |
| `Ready` | Every backend check passed and every component Deployment finished rolling out and is available |
| `Error` | Something needs attention: rejected credentials, an invalid config or missing Secret, missing buckets or privileges, a rollout past its progress deadline, an incomplete bootstrap, worker nodes that failed to provision, a refused (unconverted or duplicate) cluster, or a Kubernetes permission error |
| `UpgradeRequired` | The cluster is behind the operator's latest revision and no target is set; nothing is changed |
| `Upgrading` | The operator is running the upgrade steps, or a step is blocked waiting for an approval or manual work |

How soon the operator checks a cluster again:

| Situation | Recheck |
|-----------|---------|
| Waiting on a backend or rollout (`Provisioning`) | every 15 seconds; a reconcile waits about 20 seconds for a rollout before requeueing |
| `Ready` with no changes | once a day, and whenever its spec or `allow-inline-config` annotation, a Secret it reads (its config, bootstrap, and Elastic CA Secrets and `kube-config`), the `banner` ConfigMap, or the availability of a component Deployment changes; every cluster is also reconciled when the operator starts |
| Worker nodes that failed to provision (`Error`) | every minute |
| `UpgradeRequired` or an invalid target | every hour, and immediately on a spec change |
| An upgrade step waiting for a backend | every 15 seconds |
| A blocked upgrade step | every minute |
| An unconverted pre-Helm cluster | every 10 minutes |
| A second `ThoriumCluster` in the Kubernetes cluster | every 5 minutes |
| Any other failed reconcile | every minute |

### Component availability

The operator watches the component Deployments (`api`, `scaler`, `baremetal-scaler`,
`event-handler`, `search-streamer`) in the namespaces it watches and reconciles their
`ThoriumCluster` when their availability changes: their replica counts, the spec generation the
Deployment controller has seen, or the status of their `Available`, `Progressing`, and
`ReplicaFailure` conditions. The operator's own patches to a Deployment's spec or annotations
don't trigger a reconcile by themselves. A deleted component Deployment is recreated.

When a component of a `Ready` cluster becomes unavailable, for example because the node running
its only pod went down, the cluster moves to `Provisioning` with `Waiting for <component> to roll
out` and the reason its pods aren't ready, such as `pod <name> is not ready although its
containers are, so its node may be down or unreachable` or a pod that can't be scheduled. The
reconcile patches every component to the spec it already has, so nothing is rolled out or
restarted; the cluster goes back to `Ready` once the component is available again (Kubernetes
replaces pods from a node that stays down after about 5 minutes).

### Nodes

With a k8s scaler, each reconcile provisions, labels, and registers the worker nodes only after
every component is deployed:

- Provisioning creates a provision pod on each available node whose pod is missing or outdated,
  up to four nodes at a time, so a node waiting on its old pod's deletion doesn't hold up the
  rest. A node that is down, unreachable, or gone from the Kubernetes API is skipped, and its
  existing provision pod is left alone.
- A node whose provision pod can't be created doesn't stop the others. The components stay
  deployed, the cluster reports `Error` with `Failed to provision nodes: node <name>: ...` (and
  any component that hasn't finished rolling out), the failed node isn't labelled, and the
  reconcile retries every minute until it succeeds.
- Labelling sets `thorium=enabled` and `thorium_version` on every provisioned, available node.
- Registration adds each node to Thorium with empty resources; its agent reports the real ones.

Between reconciles, the node watcher checks each node when it changes and every 15 minutes. It
provisions and labels a node that has no `thorium` label yet (such as a new node, or one that was
down when it was first provisioned) once the node is ready, and replaces the provision pod of a
node whose `thorium_version` differs from the version the API reports. It leaves a node labelled
`thorium=disabled`, or `thorium=enabled` with the current version, alone, and it doesn't watch
provision pods. The node watcher (and the watcher that labels the MCP API pod) only acts on a
cluster the operator applied successfully: while the cluster is held by an upgrade
(`UpgradeRequired` or an upgrade `Error`), refused as an unconverted pre-Helm deployment, or its
config can't be resolved, both leave its nodes and pods alone until the next successful apply.

The operator never unregisters a node from Thorium, and it doesn't remove the `thorium=enabled`
label from a node that goes down. This is harmless since Kubernetes doesn't schedule onto a
`NotReady` node. A node that comes back keeps its labels, so the node watcher leaves it alone; if
Kubernetes deleted its provision pod while it was down, the next reconcile of the
`ThoriumCluster` recreates it. The agent the earlier pod installed under `/opt/thorium` stays on
the node either way. The same holds for a provision pod you delete: it comes back on the next
reconcile, which restarting the operator triggers at once
(`kubectl -n thorium rollout restart deployment/operator`). The operator removes node labels only
when the `ThoriumCluster` is deleted, and never from a node labelled `thorium=disabled`, which
keeps all of its labels (`thorium_version` included) so the opt-out survives a redeploy.

## Annotations and labels

| Name | On | Meaning |
|------|----|---------|
| `thorium.sandia.gov/allow-inline-config` | `ThoriumCluster` (set by you) | `"true"` (exactly) lets a cluster keep `thorium.secret_key` inline in `spec.config` with no `config_secrets`; without it such a cluster is treated as an unconverted pre-Helm deployment. See [Moving a pre-Helm deployment without Helm](./deploy-thorium.md#moving-a-pre-helm-deployment-without-helm) |
| `thorium.sandia.gov/config-hash` | component pod templates | Hash of the rendered `thorium.yml` |
| `thorium.sandia.gov/mounts-hash` | component pod templates | Hash of everything else a component mounts: `keys`, the `banner` ConfigMap (API), the Elastic CA, and `kube-config` (scaler without a service account) |
| `thorium.sandia.gov/thorium-version` | provision pods | The Thorium version the API reported when the pod was created |
| `thorium.sandia.gov/pod-hash` | provision pods | Hash of the provision pod template |
| `thorium.sandia.gov/registry-auth` | `registry-token` and `docker-skopeo` Secrets | `"true"` marks a Secret the operator rendered from `registry_auth`, which it deletes once `registry_auth` is unset |
| `thorium.sandia.gov/state-revision`, `state-version`, `state-hash` | `thorium-upgrade-state` ConfigMap | Labels holding the recorded revision, the operator version that recorded it, and a shortened hash of the state; the `state-hash` annotation holds the full hash |
| `thorium` (`enabled`/`disabled`), `thorium_version` | nodes | Whether Thorium schedules on a node and which version its agent was provisioned with |
| `app=node-provisioner`, `thorium.sandia.gov/cluster` | provision pods | Selects every node provision pod, and those of one `ThoriumCluster` (by name) |
| `mcp=enabled` | API pods | The one live API pod the `thorium-mcp` Service routes to; the operator moves it to another API pod when that pod is deleted, stops, or its node goes down |

## Names the operator manages

All of these live in the `ThoriumCluster`'s namespace.

| Kind | Name | Notes |
|------|------|-------|
| Secret | `thorium` | The rendered config (key `thorium.yml`) every component mounts |
| Secret | `keys`, `keys-kaboom` | API keys of the `thorium` and `thorium-kaboom` users |
| Secret | `thorium-pass`, `thorium-kaboom-pass` | Passwords of those users |
| Secret | `thorium-operator-pass` | The operator's own Thorium password. Kept when the cluster is deleted, so a new `ThoriumCluster` on the same databases logs in again; delete it by hand if you wipe the databases |
| Secret | `registry-token`, `docker-skopeo` | Rendered from `registry_auth` and labelled `thorium.sandia.gov/registry-auth=true`. Deleted when `registry_auth` is unset; a Secret with either name that lacks the label (or that Helm manages) is left alone |
| Secret | `kube-config` | Read (not written): the kube config (key `config`) a scaler without a service account mounts |
| ConfigMap | `tracing-conf` | Tracing config for the components |
| ConfigMap | `banner` | Read (not written): the login banner (key `banner.txt`) the API mounts; optional |
| ConfigMap | `thorium-upgrade-state` | The recorded revision and upgrade history (key `state.json`, at most 200 history entries). Kept when the cluster is deleted |
| Service | `thorium-api`, `thorium-mcp` | Port 80. `thorium-api` selects `app=api`; `thorium-mcp` selects `app=api,mcp=enabled` |
| Deployment | `api`, `scaler`, `baremetal-scaler`, `event-handler`, `search-streamer` | One per component |
| Pod | provision pods (`app=node-provisioner`) | One per scheduled node; runs `/app/thoradm` to install the agent under `/opt/thorium` |
| Finalizer | `thoriumclusters.sandia.gov` | On the `ThoriumCluster`; removed after cleanup |

Config, bootstrap, and CA Secrets can't use a reserved name: `thorium`, `keys`, `keys-kaboom`,
`docker-skopeo`, `registry-token`, `thorium-image-pull`, or any name ending in `-pass`.

Deleting a `ThoriumCluster` while its operator runs removes the node labels (when it has a k8s
scaler; nodes labelled `thorium=disabled` keep theirs), the provision pods, the component
Deployments, the `thorium-api` and `thorium-mcp` Services, the `thorium`, `keys`, `keys-kaboom`,
`thorium-pass`, `thorium-kaboom-pass`, `docker-skopeo`, and `registry-token` Secrets, and the `tracing-conf` ConfigMap. Databases and their contents, the
namespace, `thorium-operator-pass`, `thorium-upgrade-state`, user passwords, cluster and node
settings, and files on the nodes under `/opt/thorium` are kept.

## One deployment per Kubernetes cluster

Thorium supports one deployment per Kubernetes cluster: node labels, node provisioning, and
cluster-scoped resources are shared, and a [namespace
prefix](./concepts.md#namespaces-and-the-namespace-prefix) only renames namespaces. The operator
manages only the oldest `ThoriumCluster` it can see and sets every other one to `Error` with:

> Thorium supports one deployment per Kubernetes cluster; ThoriumCluster `<namespace>/<name>` is
> already deployed; namespace prefixes only rename namespaces. The operator changes nothing for
> this ThoriumCluster; delete it or delete `<namespace>/<name>` first (see "Namespaces and the
> namespace prefix" in the Thorium docs)

Deleting such a duplicate releases it without touching the deployed cluster. The Helm chart and
minithor refuse to install a second deployment as well.

This check only covers the `ThoriumCluster`s one operator can see. Operators each limited to
their own namespace (`--namespace`, or the chart's `operator.controller.watchOwnNamespaceOnly`)
can't see each other's clusters, so nothing stops them from each deploying one and fighting over
the same nodes. Running more than one operator per Kubernetes cluster is unsupported: run a
single operator per Kubernetes cluster. The thorium chart's install guard still refuses a second
Helm install in another namespace, since it looks for `ThoriumCluster`s in every namespace.

## Operator command line

The Thorium image's entrypoint is `./thorium-operator operate` (working directory `/app`).

```
thorium-operator operate [--url <URL>] [--namespace <NAMESPACE>]
thorium-operator crd
```

| Option | Notes |
|--------|-------|
| `--namespace <NAMESPACE>` (env `THORIUM_OPERATOR_NAMESPACE`) | Only watch `ThoriumCluster`s in this namespace. Without it the operator watches every namespace. The chart passes it while `operator.controller.watchOwnNamespaceOnly` is `true` |
| `-u, --url <URL>` | The Thorium API URL, for running the operator outside the cluster during development |

`thorium-operator crd` prints the `ThoriumCluster` CRD as YAML. At startup `operate` applies that
CRD with server-side apply (field manager `thorium_cluster_apply`) and waits for it to be
established, so the CRD always matches the running operator.

With `KUBECONFIG` set, the operator builds a client for every context in that kube config and
manages each Kubernetes cluster under its context name. Without it, the operator uses its service
account and calls its cluster `kubernetes-admin@cluster.local`. The k8s scaler's cluster entry
(`thorium.scaler.k8s.clusters`, Helm value `operator.cluster.scaler.k8s.context`) must use the same
name, or the operator reports that the nodes aren't provisioned.

The operator reaches the Thorium API at `http://thorium-api.<namespace>.svc:80`, which resolves
through the pod's DNS search path whatever the cluster domain is.

## RBAC

The chart runs the operator as the `thorium-operator` service account and the components as the
`thorium` service account. Cluster-scoped names carry the namespace (`<ns>-operator`,
`<ns>-components`) so they follow the namespace prefix.

Cluster rules of the operator (ClusterRole `<ns>-operator`):

| API group | Resources | Verbs |
|-----------|-----------|-------|
| `apiextensions.k8s.io` | `customresourcedefinitions` | `get`, `list`, `watch`, `create` |
| `apiextensions.k8s.io` | `customresourcedefinitions` named `thoriumclusters.sandia.gov` | `update`, `patch` |
| `""` | `nodes` | `get`, `list`, `watch`, `patch` |

Namespaced rules of the operator, bound in the thorium namespace (Role `thorium-operator`) while
`watchOwnNamespaceOnly` is `true`, and added to the cluster rules otherwise:

| API group | Resources | Verbs |
|-----------|-----------|-------|
| `sandia.gov` | `thoriumclusters` | `get`, `list`, `watch`, `update`, `patch` |
| `sandia.gov` | `thoriumclusters/status` | `get`, `update`, `patch` |
| `""` | `secrets` | `get`, `list`, `watch`, `create`, `update`, `patch`, `delete` |
| `""` | `pods` | `get`, `list`, `watch`, `create`, `patch`, `delete`, `deletecollection` |
| `""` | `services` | `get`, `create`, `update`, `patch`, `delete` |
| `""` | `configmaps` | `get`, `list`, `watch`, `create`, `update`, `patch`, `delete` |
| `apps` | `deployments` | `get`, `list`, `watch`, `create`, `update`, `patch`, `delete` |

Component rules (ClusterRole `<ns>-components`, bound to the `thorium` service account). Only the
k8s scaler calls the Kubernetes API, and it lists across namespaces with field selectors, so these
are cluster-wide. The chart binds this ClusterRole whether or not the k8s scaler is deployed.

| API group | Resources | Verbs |
|-----------|-----------|-------|
| `""` | `namespaces` | `list`, `create` |
| `""` | `nodes` | `list` |
| `""` | `pods` | `list`, `create`, `delete` |
| `""` | `secrets` | `get`, `list`, `create`, `update` |
| `""` | `configmaps` | `list`, `create` |
| `networking.k8s.io` | `networkpolicies` | `list`, `create`, `delete` |

To print the exact rules of a chart version:

```bash
helm template thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION -n thorium \
  --set secrets.renderOnly=true --show-only charts/operator/templates/rbac.yaml
```

Anyone who can create or edit a `ThoriumCluster` controls the images, commands, and environment
of every component and the endpoints the operator sends credentials to; with the scaler's
cluster-wide Secret read and the provision pods' root host mounts, that is roughly cluster-wide
Secret read plus root on the nodes. Grant it like `cluster-admin`.

## Revision catalog

| Revision | Released with | Description | Steps |
|----------|---------------|-------------|-------|
| `2026-10-v01` | 1.8.1 | Thorium deployed by the minithor or megathor scripts before the Helm charts (the baseline every converted cluster starts from) | none |
| `2026-10-v02` | 1.8.1 | Thorium deployed by the Helm charts with the chart-managed operator | `scylla-role` (Infra), `elastic-identity` (Infra), `elastic-reindex-keyword-mappings` (Data), `finalize` (Config) |

`thoradm migrate list` prints the catalog of the build you run. See [Upgrading
Thorium](./upgrades.md) for what each step does.

## Known limitations

- The operator applies its own CRD at startup, so rolling the operator back to an older version
  also rolls the cluster-scoped CRD back.
- The operator doesn't perform `Data` or `Manual` steps itself and never takes backups: an
  approved step that needs work describes the manual procedure and waits for it. The `quiesce`
  list of a step is informational; nothing is scaled down for you.
- The chart-managed Elasticsearch's TLS certificate isn't verified, since ECK's CA lives in a
  namespace the operator can't read. An external Elasticsearch can be verified with
  `elastic_ca_secret`.
- After the Elastic CA Secret is rotated, the operator sees the new CA only once the kubelet
  refreshes its mounted file (typically up to about a minute), so the status may briefly show an
  Elastic TLS error.
- The Scylla operator's webhook certificate is generated by the infra-operators chart (valid
  10 years) instead of by cert-manager.
