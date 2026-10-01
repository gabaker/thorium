# Deploy Thorium with Helm

Thorium publishes two Helm charts that deploy Thorium and everything it depends on onto an
existing Kubernetes cluster. This is the recommended way to deploy Thorium; the
[manual steps](./deploy.md#install-infrastructure-components) remain available if you need to
install each component yourself.

| Chart | Installed | Contents |
|-------|-----------|----------|
| `infra-operators` | once per cluster | The Scylla, Elastic Cloud on Kubernetes (ECK), and Kubegres operators that manage Thorium's databases |
| `thorium` | once per Thorium instance | Generated credentials, Redis, Scylla, Elasticsearch/Kibana, SeaweedFS (S3), Quickwit and Jaeger (tracing), the Thorium operator, and the `ThoriumCluster` it reconciles |

Both are published as OCI charts:

```
oci://ghcr.io/cisagov/thorium/charts/infra-operators
oci://ghcr.io/cisagov/thorium/charts/thorium
```

## Prerequisites

- A Kubernetes cluster with a default `StorageClass` (or set `infra.storageClass`).
- Helm 3.8 or newer (for OCI charts).
- Kernel settings on every node that runs Scylla or Elasticsearch:

  ```
  fs.aio-max-nr=2097152
  vm.max_map_count=262144
  ```

- An ingress controller if you want to reach Thorium through an ingress (nginx and Traefik
  are supported).

## Install

```bash
VERSION=1.8.1
# the operators, once per cluster
helm install infra-operators oci://ghcr.io/cisagov/thorium/charts/infra-operators \
  --version $VERSION -n infra-operators --create-namespace --wait
# a Thorium instance
helm install thorium oci://ghcr.io/cisagov/thorium/charts/thorium \
  --version $VERSION -n thorium --create-namespace -f site.yaml
# wait for the Thorium operator to finish setting up Thorium
kubectl -n thorium wait thoriumcluster/thorium --for=jsonpath='{.status.phase}'=Ready --timeout=30m
```

Once ready, read the initial admin user's credentials:

```bash
kubectl -n thorium get secret thorium-admin -o jsonpath='{.data.username}' | base64 -d; echo
kubectl -n thorium get secret thorium-admin -o jsonpath='{.data.password}' | base64 -d; echo
```

If a cluster already runs one of the operators, turn it off in `infra-operators`, for example
`--set eck-operator.enabled=false`.

## How the thorium chart works

The `thorium` chart is an umbrella over four subcharts. Each top-level key in your values file
configures one of them:

| Key | Subchart | Purpose |
|-----|----------|---------|
| `secrets` | secrets | Creates the instance's namespaces and a `thorium-credentials` secret holding every credential, then renders each service's own secrets from it |
| `infra` | infra | Redis, SeaweedFS, the `ScyllaCluster`, Elasticsearch/Kibana, Quickwit's Postgres metastore, Jaeger, and an optional container registry |
| `quickwit` | quickwit | Quickwit trace storage |
| `operator` | operator | The Thorium operator and the `ThoriumCluster` resource |
| `global` | all | Settings shared by every subchart (`clusterDomain`, `imageRegistry`) |

Credentials you don't set are generated on the first install and read back on every upgrade,
so they never change. To use your own, set them under `secrets.credentials` (for example
`thoriumSecretKey`, `s3AccessKey`, `s3SecretKey`, `adminPassword`).

The `ThoriumCluster` the chart creates contains no secrets. The Thorium operator merges the
credentials in from secrets, creates Thorium's Scylla and Elasticsearch users, deploys the
Thorium API, scaler, event handler and search streamer, and then creates the initial admin
user. Its progress is shown in the resource's status:

```bash
kubectl -n thorium get thoriumcluster thorium
```

The chart's defaults are sized for a single-node test cluster. The chart ships a
`values-production.yaml` with a starting point for multi-node clusters (replicated Scylla
and Elasticsearch, a replicated S3 store, Traefik with TLS); copy it and adjust sizes, node
names, and hostnames. A minimal `site.yaml` might look like:

```yaml
infra:
  storageClass: fast-ssd
operator:
  ingress:
    type: nginx
    host: thorium.example.com
    tls:
      secretName: thorium-tls
  cluster:
    scaler:
      # the nodes Thorium may run analysis jobs on (empty means every node)
      nodes: [worker-1, worker-2, worker-3]
```

See `helm show values oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION` and each
subchart's `values.yaml` for every setting.

### Using an existing S3 store

To use an S3 service you already run (such as a Rook/Ceph object store) instead of SeaweedFS:

```yaml
infra:
  seaweedfs:
    enabled: false
secrets:
  consumers:
    seaweedfs: false
  quickwitS3Endpoint: http://s3.example.com
  credentials:
    s3AccessKey: <access key>
    s3SecretKey: <secret key>
operator:
  backends:
    s3:
      endpoint: http://s3.example.com
```

Create the bucket Quickwit stores traces in (`quickwit` by default) before installing; Thorium
creates its own buckets.

## Instances and namespaces

An instance is installed into the namespace `thorium` or a prefixed one such as `b-thorium`.
The chart derives a prefix from that namespace and uses it for every namespace the instance
creates:

| Release namespace | Namespaces used |
|-------------------|-----------------|
| `thorium` | `thorium`, `redis`, `scylla`, `elastic`, `seaweedfs`, `quickwit`, `jaeger` |
| `b-thorium` | `b-thorium`, `b-redis`, `b-scylla`, `b-elastic`, `b-seaweedfs`, `b-quickwit`, `b-jaeger` |

Several instances can share a cluster this way. Each runs its own Thorium operator, which only
manages its own namespace; the `infra-operators` chart is shared. The Thorium Kubernetes
scaler creates namespaces named after Thorium groups, so run the scaler in only one instance
per cluster (`operator.cluster.components.scaler: null` in the others).

## Offline (air-gapped) installs

1. On a connected machine, pull both charts:

   ```bash
   helm pull oci://ghcr.io/cisagov/thorium/charts/infra-operators --version $VERSION
   helm pull oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION
   ```

2. List the images the charts deploy and mirror them into your registry under their upstream
   paths without the registry host (for example `docker.io/scylladb/scylla:6.2.3` becomes
   `<registry>/scylladb/scylla:6.2.3`). The repository's `deploy/charts/list-images.sh`
   prints each image and its mirror path; pass it the values files you will deploy with.

3. Install from the packaged charts, pointing every image at the mirror:

   ```bash
   REGISTRY=10.0.0.1:5000
   helm install infra-operators ./infra-operators-$VERSION.tgz -n infra-operators --create-namespace --wait \
     --set global.imageRegistry=$REGISTRY \
     --set scylla-operator.image.repository=$REGISTRY/scylladb \
     --set eck-operator.image.repository=$REGISTRY/eck/eck-operator
   helm install thorium ./thorium-$VERSION.tgz -n thorium --create-namespace -f site.yaml \
     --set global.imageRegistry=$REGISTRY
   ```

`global.imageRegistry` also covers the images the operators start (Elasticsearch, Kibana,
Scylla, and the Thorium components). Tool images referenced by a toolbox must be mirrored
separately.

## Importing a toolbox

The chart can import a [toolbox](../../developers/toolbox.md) once Thorium is ready. Set
`operator.toolbox.url` to a `toolbox.json` URL the cluster can reach, or pass the file itself:

```bash
helm upgrade thorium <chart> -n thorium --reuse-values --set-file operator.toolbox.json=./toolbox.json
```

The import creates the groups and network policies the toolbox references first
(`operator.toolbox.groups` and `operator.toolbox.networkPolicies`).

## Upgrades and uninstalling

Upgrade with `helm upgrade` and the same values. To remove an instance, delete its
`ThoriumCluster` first so the operator can clean up, then uninstall:

```bash
kubectl -n thorium delete thoriumcluster thorium
helm uninstall thorium -n thorium
```

Namespaces, volumes, and the `thorium-credentials` secret are kept after uninstalling; delete
the namespaces to remove the data.
