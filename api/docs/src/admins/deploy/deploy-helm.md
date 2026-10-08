# Install with Helm

The Thorium Helm charts install Thorium and every backing service it needs onto an existing
Kubernetes cluster with two `helm install` commands. This is the recommended way to deploy
Thorium; megathor and minithor install the same charts. Read
[Deployment Concepts](./concepts.md) and [Planning a Deployment](./planning.md) first, and see
[Configure the Helm Charts](./helm-configuration.md) for everything you can set.

| Chart | Installed | Contents |
|-------|-----------|----------|
| `infra-operators` | Once per cluster, into `infra-operators` | The Scylla, Elastic Cloud on Kubernetes (ECK), and Kubegres operators that manage Thorium's databases |
| `thorium` | Once per cluster, into the thorium namespace | Credentials, Redis, Scylla, Elasticsearch/Kibana, SeaweedFS (S3), Quickwit and Jaeger (tracing), the Thorium operator, and the `ThoriumCluster` it reconciles |

Both are published as OCI charts:

```
oci://ghcr.io/cisagov/thorium/charts/infra-operators
oci://ghcr.io/cisagov/thorium/charts/thorium
```

## Prerequisites

- A Kubernetes cluster with a default `StorageClass` (or set `infra.storageClass`), and the
  kernel settings from [Planning a Deployment](./planning.md#kubernetes-cluster) on the nodes
  that run Scylla and Elasticsearch.
- Helm 3.8 or newer and `kubectl`.
- No other Thorium deployment on the cluster.

## Install

```bash
VERSION=1.8.1
# the operators, once per cluster
helm install infra-operators oci://ghcr.io/cisagov/thorium/charts/infra-operators \
  --version $VERSION -n infra-operators --create-namespace --wait
# Thorium itself
helm install thorium oci://ghcr.io/cisagov/thorium/charts/thorium \
  --version $VERSION -n thorium --create-namespace -f site.yaml
# wait for the Thorium operator to finish setting up Thorium
kubectl -n thorium wait thoriumcluster/thorium --for=jsonpath='{.status.phase}'=Ready --timeout=30m
```

Install into `<prefix>-thorium` instead of `thorium` (for example `-n dev-thorium`) to prefix
every Thorium namespace (see
[Namespaces and the namespace prefix](./concepts.md#namespaces-and-the-namespace-prefix)); any
other namespace fails to render. Without a `site.yaml`, the chart's single-node defaults apply.

A fresh install spends several minutes in the `Provisioning` phase while the backing services
start. Follow it with:

```bash
kubectl -n thorium get thoriumcluster -o wide
```

If the phase becomes `Error`, the message names the problem; see
[Troubleshooting Deployments](./troubleshooting.md).

If the cluster already runs one of the operators, turn it off in the infra-operators chart with
`--set scylla-operator.enabled=false`, `--set eck-operator.enabled=false`, or
`--set kubegres.enabled=false`.

## A minimal site.yaml

```yaml
infra:
  storageClass: fast-ssd
operator:
  ingress:
    type: nginx
    host: thorium.example.com
    tls:
      # an existing kubernetes.io/tls Secret in the thorium namespace
      secretName: thorium-tls
  cluster:
    # accept API requests only from the UI the API serves (list other origins in
    # operator.cluster.config.thorium.cors.domains)
    cors:
      insecure: false
    scaler:
      k8s:
        # the nodes Thorium may run analysis jobs on (an empty list means every node)
        nodes: [worker-1, worker-2, worker-3]
```

For a multi-node cluster, start from the chart's [`values-production.yaml`](./example-production.md) (see
[Sizing](./planning.md#sizing)) and layer your own file over it:

```bash
helm install thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION \
  -n thorium --create-namespace -f thorium/values-production.yaml -f site.yaml
```

## First login

The operator creates the initial admin user from the `thorium-admin` Secret. Its username is
`secrets.credentials.adminUsername` (default `admin`) and its password is generated unless you set
`secrets.credentials.adminPassword`:

```bash
kubectl -n thorium get secret thorium-admin -o jsonpath='{.data.username}' | base64 -d; echo
kubectl -n thorium get secret thorium-admin -o jsonpath='{.data.password}' | base64 -d; echo
```

Open the host you configured for the ingress, or forward the API to your workstation and open
`http://localhost:8080`:

```bash
kubectl -n thorium port-forward svc/thorium-api 8080:80
```

Then install [Thorctl](../../getting_started/thorctl.md) from the API and log in with the same
credentials.

## Importing a toolbox

The chart can import a [toolbox](../../developers/toolbox.md) once Thorium is up. Set
`operator.toolbox.url` to a `toolbox.json` URL the cluster can reach, or pass the file itself:

```bash
helm upgrade thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION \
  -n thorium --reuse-values --set-file operator.toolbox.json=./toolbox.json
```

The import Job logs in as the admin from `operator.cluster.bootstrap.admin.secret`, creates the
groups and network policies the toolbox references first (`operator.toolbox.groups` and
`operator.toolbox.networkPolicies`), and then imports with `operator.toolbox.mode`
(`skip-conflicts` or `overwrite`); `operator.toolbox.groupOverride` puts every tool in one group.
The Job is named after a hash of its whole spec, so changing any of these settings, or anything
else the Job uses (the Thorium image, pull settings, the admin secret, scheduling), runs a new
import; with `skip-conflicts` a repeated import changes nothing. Set `operator.toolbox.enabled:
false` to never import a toolbox, even with `url` or `json` set. See
[Chart Values Reference](./chart-values.md) for the remaining `operator.toolbox` values.

## Offline installs

1. On a connected machine, pull both charts:

   ```bash
   helm pull oci://ghcr.io/cisagov/thorium/charts/infra-operators --version $VERSION
   helm pull oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION
   ```

2. List the images the charts deploy and mirror them into your registry under their upstream
   paths without the registry host (for example `docker.io/scylladb/scylla:6.2.3` becomes
   `<registry>/scylladb/scylla:6.2.3`). The script `deploy/charts/scripts/list-images.sh` in
   the Thorium source repository prints each image and its mirror path for the thorium chart
   values files you pass it; list the infra-operators values files in the
   `INFRA_OPERATORS_VALUES` environment variable. megathor's `scripts/mirror-images.bash` reads
   its output and pushes every image to a registry.

3. Install from the packaged charts, pointing every image at the mirror:

   ```bash
   REGISTRY=10.0.0.1:5000
   helm install infra-operators ./infra-operators-$VERSION.tgz -n infra-operators --create-namespace --wait \
     --set global.imageRegistry=$REGISTRY
   helm install thorium ./thorium-$VERSION.tgz -n thorium --create-namespace -f site.yaml \
     --set global.imageRegistry=$REGISTRY
   ```

On the infra-operators chart, `global.imageRegistry` moves the Scylla, ECK, and Kubegres operator
images to the mirror, and renders the Scylla operator's `ScyllaOperatorConfig` (named `cluster`)
so its auxiliary images, the ScyllaDB utilities and Bash tools images
(`scyllaOperatorConfig.scyllaDBUtilsImage` and `scyllaOperatorConfig.bashToolsImage`), come from
the mirror too. The operator only runs those for `NodeConfig` node tuning and `ScyllaDBMonitoring`,
which Thorium doesn't create, but `list-images.sh` lists them so the mirror is complete. If the
infra-operators release was first installed without a mirror, the operator has already created
`cluster`, so pass `--take-ownership` to the `helm upgrade` that adds `global.imageRegistry`
(megathor adopts it for you). On the thorium chart `global.imageRegistry` covers every image,
including the ones the operators start (Elasticsearch, Kibana, Scylla, and the Thorium
components). Tool images
referenced by a toolbox must be mirrored separately. If the registry needs credentials, see
[Image pull secrets](./helm-configuration.md#image-pull-secrets); where the registry is
unreachable but the images are cached on the nodes, set `operator.image.pullPolicy: IfNotPresent`.

## Upgrading

Upgrade with `helm upgrade` and the same values, then follow the steps in
[Upgrading Thorium](./upgrades.md) if the deployment reports `UpgradeRequired`:

```bash
helm upgrade thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION \
  -n thorium -f site.yaml
```

## Uninstalling

```bash
helm uninstall thorium -n thorium
```

A `pre-delete` hook deletes the `ThoriumCluster` first and waits for the operator to clean up.
Namespaces, volumes, and the `thorium-credentials` Secret are kept. See
[Uninstalling](./operate.md#uninstalling) for what is kept, how to remove the data, and what to do
when the hook times out.
