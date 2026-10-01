# Deploy Thorium with Helm

Thorium publishes two Helm charts that deploy Thorium and everything it depends on onto an
existing Kubernetes cluster. This is the recommended way to deploy Thorium; the
[manual steps](./deploy.md#install-infrastructure-components) remain available if you need to
install each component yourself.

| Chart | Installed | Contents |
|-------|-----------|----------|
| `infra-operators` | once per cluster | The Scylla, Elastic Cloud on Kubernetes (ECK), and Kubegres operators that manage Thorium's databases |
| `thorium` | once per cluster | Generated credentials, Redis, Scylla, Elasticsearch/Kibana, SeaweedFS (S3), Quickwit and Jaeger (tracing), the Thorium operator, and the `ThoriumCluster` it reconciles |

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
# Thorium itself
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
| `secrets` | secrets | Creates Thorium's namespaces and a `thorium-credentials` secret holding every credential, then renders each service's own secrets from it |
| `infra` | infra | Redis, SeaweedFS, the `ScyllaCluster`, Elasticsearch/Kibana, Quickwit's Postgres metastore (each only when `global.managed` keeps it), Jaeger, and an optional container registry |
| `quickwit` | quickwit | Quickwit trace storage |
| `operator` | operator | The Thorium operator and the `ThoriumCluster` resource |
| `global` | all | Settings shared by every subchart (`clusterDomain`, `imageRegistry`, the `managed` service toggles, `elasticIndices`) |

Credentials you don't set are generated on the first install and read back on every upgrade,
so they never change. To use your own, set them under `secrets.credentials` (for example
`thoriumSecretKey`, `s3AccessKey`, `s3SecretKey`, `adminPassword`).

> **Never change `thoriumSecretKey` on an existing deployment.** It peppers every user's password
> hash, so changing it locks out every user, including the operator's own.
>
> **Back up the `thorium-credentials` secret.** Generated credentials are read back with Helm
> `lookup`. New values would break every backend still using the old ones and, for
> `thoriumSecretKey`, lock out every user, so the chart refuses to generate them when it can't
> read the old ones:
>
> - When `lookup` can't reach the cluster (`helm template`, client-side `--dry-run`, GitOps
>   renderers such as Argo CD or Flux), rendering fails unless every credential is set under
>   `secrets.credentials`: `thoriumSecretKey`, `redisPassword`, `scyllaPassword`,
>   `elasticPassword`, `s3AccessKey`, `s3SecretKey`, `adminPassword`, plus `postgresPassword`
>   with `global.managed.postgres` and `registryPassword` with `secrets.registryAuth.username`.
>   GitOps deployments should supply these from their own secret store.
> - On `helm upgrade` with no `thorium-credentials` secret, rendering fails and asks you to
>   restore it from your backup, unless every credential is set or `secrets.allowRegenerate: true`
>   is set (only for a deployment with no data to keep).
> - For render-only checks whose output is never applied (CI, image listing), set
>   `secrets.renderOnly: true` to render placeholder credentials. It fails during a real
>   `helm install`/`upgrade`.

The `ThoriumCluster` the chart creates contains no secrets. The Thorium operator merges the
credentials in from secrets, creates Thorium's buckets and Scylla role, checks that Thorium's
credentials work against Elasticsearch (with the index privileges Thorium needs), Scylla, and
Redis before writing its config, deploys the Thorium API, scaler, event handler and search
streamer, and then creates the initial admin user. The search streamer creates Thorium's
Elasticsearch indexes when it starts and fills new ones from the database. The resource only reports `Ready` once every component has finished rolling out;
while components roll out it is `Provisioning` with a message naming them, and the message adds
the reason of any failing container (a crash loop, an image pull error, or a non-zero exit). The chart's Elasticsearch gets Thorium's user and role from ECK's file realm, so the
operator never uses the `elastic` superuser. Its progress is shown in the resource's status
(`Provisioning`, `Ready`, or `Error`):

```bash
kubectl -n thorium get thoriumcluster thorium
```

- `Provisioning`: the operator is deploying, or waiting for something that will finish on its
  own: a backend that isn't reachable or accepting connections yet (Scylla, Redis, S3, or
  Elasticsearch, including "Waiting for Elastic to accept Thorium's credentials" while ECK
  applies Thorium's user) or components still rolling out. The message says what it is waiting
  on and the operator checks again every 15 seconds. A fresh install spends several minutes here.
- `Ready`: every backend check passed and every component finished rolling out.
- `Error`: something needs attention: credentials a reachable backend rejected, an invalid
  config or missing config Secret, missing buckets with `skip_bucket_auto_create`, missing
  Elasticsearch privileges, a rollout past its progress deadline, a missing bootstrap admin
  Secret, or a Kubernetes permission error. The message names the problem.

Thorium components roll out when the rendered config, anything else they mount (their keys,
the login banner, the Elasticsearch CA, the scaler's kube config), or their own spec changes. A new image pushed under the same tag changes none of these, so
restart the components to pick it up:

```bash
kubectl -n thorium rollout restart deployment
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

### External services

Every backing service can run outside the chart (for example a cloud service). Set its
`global.managed` toggle to `false`, point `operator.backends` at it, and supply the credentials
it issued under `secrets.credentials`. Rendering fails with a message naming any missing value.
(`infra.<service>.enabled` and `secrets.consumers` are not chart values; rendering fails when one
is set, naming the `global.managed` toggle to use instead.)

| Service | Toggle | When managed, the chart | When external, you provide | Runs automatically either way |
|---------|--------|-------------------------|----------------------------|-------------------------------|
| Scylla | `global.managed.scylla` | deploys Scylla; the operator creates Thorium's role with the default `cassandra` login | `operator.backends.scylla.nodes`, and Thorium's role (see below); without the opt-in bootstrap, `secrets.credentials.scyllaUsername`/`scyllaPassword` set to the role you created (required) | the operator checks Thorium's login; the API creates its keyspace |
| Elasticsearch | `global.managed.elastic` | deploys Elasticsearch/Kibana with Thorium's user in ECK's file realm | `operator.backends.elastic.node`, optionally `caSecret`, and Thorium's user (see below); without the opt-in bootstrap, `secrets.credentials.elasticUsername`/`elasticPassword` set to the user you created (required) | the operator verifies Thorium's credentials and its `view_index_metadata`, `write`, and `read` privileges on every index (plus `create_index` on indexes that don't exist yet); the search streamer creates the indexes |
| Redis | `global.managed.redis` | deploys Redis | `operator.backends.redis.host` and `secrets.credentials.redisPassword` | the operator checks `AUTH` and `PING` |
| S3 | `global.managed.s3` | deploys SeaweedFS and creates the `quickwit` bucket | `operator.backends.s3.endpoint`, `secrets.credentials.s3AccessKey`/`s3SecretKey`, and with Quickwit `secrets.quickwitS3Endpoint` and the `quickwit` bucket | the operator creates Thorium's buckets |
| Postgres | `global.managed.postgres` | deploys Postgres for Quickwit's metastore and creates its database | with Quickwit, `secrets.quickwitMetastoreUri` (e.g. `postgres://user:password@host:5432/quickwit-metastore`) for a database you created | nothing |

For example, to use an S3 service you already run (such as a Rook/Ceph object store) instead of
SeaweedFS:

```yaml
global:
  managed:
    s3: false
secrets:
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
creates its own buckets. If Thorium's S3 identity may not create buckets, pre-create them and set
`operator.cluster.config.thorium.s3.skip_bucket_auto_create: true`: the operator then creates
nothing, but still checks every required bucket (files, repos, attachments, results, ephemeral,
graphics, and reaction cache) with `HeadBucket` on each reconcile and reports one error naming
the S3 endpoint and every missing or inaccessible bucket.

On every reconcile, before writing its config or rolling out any component, the operator
creates any missing S3 buckets (existing buckets need no create rights), authenticates to
Elasticsearch as Thorium's own user, checks which configured indexes exist (`HEAD /<index>`,
which needs `view_index_metadata`), checks that it holds `view_index_metadata`, `write`, and
`read` on every configured index plus `create_index` on each index that doesn't exist yet, and
checks that Thorium's own credentials log in to Scylla and Redis. It creates no Elasticsearch indexes; the search
streamer does. With the chart's Elasticsearch the first reconciles may report "Waiting for
Elastic to accept Thorium's credentials" (phase `Provisioning`) until ECK has applied Thorium's
user; the operator checks again on its own. Thorium's own identities must either already exist or be created by the
operator from an admin Secret you opt into. Admin Secrets are always read from the thorium
namespace (`thorium` or `<prefix>-thorium`):

- **Scylla**: pre-create Thorium's role as a superuser (the API creates its keyspace) and set
  `secrets.credentials.scyllaUsername` and `scyllaPassword` to it, or set
  `operator.cluster.bootstrap.scylla.enabled: true` with `adminSecret` naming a Secret that
  holds superuser credentials. Without one the operator tries the default
  `cassandra`/`cassandra` login and reports in the status that an admin Secret is required
  if Scylla rejects it; a Scylla that isn't accepting connections yet is reported as "Waiting
  for Scylla to accept connections".
- **Elasticsearch**: pre-create Thorium's user with at least `view_index_metadata`, `write`,
  and `read` on every index in `global.elasticIndices`, plus `create_index` on any of those
  indexes you don't pre-create (the search streamer creates them). With every index pre-created
  `create_index` isn't needed. `all` on `thorium*` covers the defaults; the search streamer's
  `--reindex` also needs `delete_index`. Then set `secrets.credentials.elasticUsername` and `elasticPassword` to it, or set
  `operator.cluster.bootstrap.elastic.adminSecret.name` to a Secret holding superuser
  credentials.

Both usernames default to `thorium`. A generated password would match no role or user you
created yourself, so without the matching bootstrap the chart refuses to render: an external
Scylla without `bootstrap.scylla` fails with "external Scylla without bootstrap.scylla needs
secrets.credentials.scyllaUsername/scyllaPassword for the role you created", and an external
Elasticsearch without `bootstrap.elastic` (on only with `adminSecret.name`) fails the same way
for `elasticUsername`/`elasticPassword`. Credentials already stored in `thorium-credentials`
satisfy the check on later upgrades, and `secrets.renderOnly` placeholders satisfy it for
render-only tooling, which never installs; every real install and upgrade enforces it. With the bootstrap enabled the password
may be left empty and is generated, since the operator creates the role or user with it. A
password that is set but wrong still shows up as a rejected login in the status (`Error`).

```yaml
global:
  managed:
    elastic: false
operator:
  backends:
    elastic:
      node: https://elastic.example.com:9200
  cluster:
    bootstrap:
      elastic:
        adminSecret:
          name: elastic-admin
          username: elastic
          passwordKey: password
```

These privileged steps run again only when `operator.cluster.bootstrap`, the Thorium config,
or one of the Secrets they name changes. The admin Secrets may be deleted after setup: each step
reads its Secret only when it has work to do (Thorium's Scylla role can't log in, Thorium's
Elasticsearch user can't authenticate with the privileges it needs, or the admin user doesn't
exist yet). If a step needs a deleted Secret, the components still roll out and the
`ThoriumCluster` reports `Error` naming the Secret until it exists again.

#### Elasticsearch certificates

The chart's own Elasticsearch serves a self-signed ECK certificate whose CA lives in
`<prefix>-elastic`, so Thorium reaches it without verifying the certificate
(`operator.backends.elastic.insecureCertificates: true`); that traffic never leaves the
cluster. To verify an external Elasticsearch, create a Secret in the thorium namespace holding
the PEM CA that signed its certificate and point `caSecret` at it:

```bash
kubectl -n thorium create secret generic elastic-ca --from-file=ca.crt=./elastic-ca.pem
```

```yaml
operator:
  backends:
    elastic:
      node: https://elastic.example.com:9200
      caSecret:
        name: elastic-ca
        # the key holding the CA (default ca.crt)
        key: ca.crt
```

The chart mounts the CA read-only at `/etc/thorium/elastic-ca/ca.crt` in the operator, and the
operator mounts it in every Thorium component. thorium.yml then sets
`cert_validation: {Full: /etc/thorium/elastic-ca/ca.crt}` and `insecure_certificates: false`, so
the certificate chain and the hostname in `node` are both verified. Without `caSecret`, an
external Elasticsearch keeps `insecureCertificates` (unverified by default) and the install
notes warn about it. The API, the operator, and the search streamer all verify it the same way.
Components read the CA when they start, and the operator rolls them out when its content
changes.

After you rotate the CA Secret, the operator reads the new CA from its mounted file, which the
kubelet refreshes with a delay (typically up to about a minute). Until then the
`ThoriumCluster` status may briefly show an Elastic TLS error; it recovers on its own, and the
components roll out with the new CA through the mounts hash.

The Quickwit bucket and Postgres metastore database are created by the chart only for its own
SeaweedFS and Postgres; create them yourself when using external services (the operator never
creates them).

Anyone who can create or edit a `ThoriumCluster` controls the images, commands, arguments, and
environment of every Thorium component, as well as the endpoints the operator sends credentials
to. The Thorium scaler runs with cluster-wide Secret read and node provision pods mount host
paths as root, so permission to edit a `ThoriumCluster` is roughly equivalent to cluster-wide
Secret read plus root on the nodes. Grant it like `cluster-admin`.

## Namespaces and the namespace prefix

A cluster runs one Thorium deployment. Install the chart into the namespace `thorium`, or into
`<prefix>-thorium` (such as `dev-thorium`) to give every Thorium namespace the namespace prefix
`<prefix>-`. The prefix must be a lowercase DNS label of at most 32 characters, like megathor's
`namespace_prefix`; the chart derives it from the release namespace:

| Release namespace | Namespaces used |
|-------------------|-----------------|
| `thorium` | `thorium`, `redis`, `scylla`, `elastic`, `seaweedfs`, `quickwit`, `jaeger` |
| `dev-thorium` | `dev-thorium`, `dev-redis`, `dev-scylla`, `dev-elastic`, `dev-seaweedfs`, `dev-quickwit`, `dev-jaeger` |

Thorium never creates a group namespace in any of these (with the prefix applied), in
`infra-operators`, `kube-system`, `kube-public`, `kube-node-lease`, or `default`, or in its
built-in defaults (`thorium`, `scylla`, `scylla-operator`, `cert-manager`, `redis`,
`elastic-system`, `jaeger`, `quickwit`): the chart renders them into
`thorium.namespace_blacklist`. Add more with `operator.cluster.namespaceBlacklist`, or replace the
whole list with `operator.cluster.config.thorium.namespace_blacklist`.

`operator.operator.watchOwnNamespaceOnly` (default `true`) is a security scope: the Thorium
operator only reconciles `ThoriumCluster`s in the thorium namespace, and its permissions on
Secrets, pods, services, ConfigMaps, and deployments are bound in that namespace instead of
granted cluster-wide. Set it to `false` only if the operator must manage a `ThoriumCluster` in
another namespace.

## Proxies

`operator.operator.proxy` (`httpProxy`, `httpsProxy`, `noProxy`) sets the proxy environment of the
Thorium operator. When `httpProxy` or `httpsProxy` is set, the operator's `no_proxy`/`NO_PROXY` is
the cluster's own ranges and names followed by your `noProxy` entries:

- `localhost`, `127.0.0.1`, `.svc`, `.svc.<clusterDomain>`, and `<clusterDomain>`
  (`global.clusterDomain`, default `cluster.local`)
- every CIDR in `global.clusterCIDRs` (default empty)

Set `global.clusterCIDRs` to the cluster's service and pod CIDRs, which the chart can't detect
(the install notes warn when a proxy is set without them).
In-cluster traffic addressed by IP (a Service's ClusterIP or a pod IP, rather than a `.svc`
name) otherwise matches none of the names above and is sent through the proxy, which usually
can't reach it:

```yaml
global:
  # the kubeadm/minikube defaults; use your cluster's --service-cluster-ip-range and pod CIDR
  clusterCIDRs: [10.96.0.0/12, 10.244.0.0/16]
operator:
  operator:
    proxy:
      httpsProxy: http://proxy.example.com:3128
      # appended after the defaults above
      noProxy: .corp.example.com
```

Nothing else in the cluster's network (such as the rest of `10.0.0.0/8`) bypasses the proxy
unless you add it to `noProxy`. The Thorium components get their proxy settings from their own
`env` in `operator.cluster.components`, which the chart doesn't fill in.

## Pod Security

The Thorium image has no `USER`, so the operator runs as root by default. It only makes network
calls and writes nothing to disk, so its container drops every capability and disallows
privilege escalation by default (`operator.operator.securityContext`), and its pod uses the
runtime's `RuntimeDefault` seccomp profile (`operator.operator.podSecurityContext`). To admit it
under the `restricted` Pod Security level, also run it as non-root:

```yaml
operator:
  operator:
    podSecurityContext:
      runAsNonRoot: true
      runAsUser: 65534
      runAsGroup: 65534
      seccompProfile:
        type: RuntimeDefault
    securityContext:
      readOnlyRootFilesystem: true
```

The uninstall hook already meets `restricted` (see "Upgrades and uninstalling"). The Thorium
components the operator deploys are configured through the `ThoriumCluster`, not these values.

## Offline (air-gapped) installs

1. On a connected machine, pull both charts:

   ```bash
   helm pull oci://ghcr.io/cisagov/thorium/charts/infra-operators --version $VERSION
   helm pull oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION
   ```

2. List the images the charts deploy and mirror them into your registry under their upstream
   paths without the registry host (for example `docker.io/scylladb/scylla:6.2.3` becomes
   `<registry>/scylladb/scylla:6.2.3`). The repository's `deploy/charts/scripts/list-images.sh`
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

The Thorium image is pulled with `imagePullPolicy: Always` by default, so a new image pushed under
the same tag is picked up whenever a pod starts. `operator.image.pullPolicy` sets the policy for
the operator and, through the ThoriumCluster, every Thorium component; set it to `IfNotPresent`
where the registry can't be reached but the image is already cached on the nodes.

If your registry needs credentials, set `operator.imagePullSecret.create: true` and
`operator.imagePullSecret.dockerconfigjson` (rendered as the `thorium-image-pull` secret), or
`operator.imagePullSecret.name` to an existing pull secret in the thorium namespace. The
operator, the chart's jobs, and every Thorium component use it. `operator.cluster.registryAuth`
(tool image registries) is rendered by the operator into its own `registry-token` secret, which
every Thorium pod also references.

## Importing a toolbox

The chart can import a [toolbox](../../developers/toolbox.md) once Thorium is ready. Set
`operator.toolbox.url` to a `toolbox.json` URL the cluster can reach, or pass the file itself:

```bash
helm upgrade thorium <chart> -n thorium --reuse-values --set-file operator.toolbox.json=./toolbox.json
```

The import creates the groups and network policies the toolbox references first
(`operator.toolbox.groups` and `operator.toolbox.networkPolicies`).

## Upgrades and uninstalling

Upgrade with `helm upgrade` and the same values.

The `ThoriumCluster`'s status message notes any Elasticsearch index that doesn't map `group` as a
keyword, such as one created from dynamic mappings. If Thorium's user was created in Elastic's
native realm, remove that `thorium` user and role with the `elastic` superuser
(`DELETE _security/user/thorium` and `DELETE _security/role/thorium`), and reindex a flagged
index by running the search streamer once with `--reindex` (add it to
`operator.cluster.components.search_streamer.args`, wait for the reindex to finish, then remove
it); this deletes and recreates the indexes and streams every document from the database again.

To remove Thorium, uninstall it:

```bash
helm uninstall thorium -n thorium
```

A `pre-delete` hook deletes the `ThoriumCluster` first and waits for the still-running operator
to clean up (node labels, provision pods, and the resources it created). Helm waits up to
`--timeout` (5 minutes by default) for the hook, so keep `operator.uninstallHook.timeoutSeconds`
(240 by default) below it or pass a larger `--timeout`.

The hook Job only runs `curl` and `jq` against the Kubernetes API with its own service account,
which may only get and delete this `ThoriumCluster`. It runs on the operator's
`operator.operator.nodeSelector` and `tolerations`, with small requests and limits
(`operator.uninstallHook.resources`), and is compatible with the `restricted` Pod Security
level: it runs as user and group 65534 with the `RuntimeDefault` seccomp profile, no privilege
escalation, every capability dropped, and a read-only root filesystem with an `emptyDir` at
`/tmp` (`operator.uninstallHook.podSecurityContext` and `securityContext` override these).

If the operator isn't running, the hook times out. `helm uninstall` takes no values, so skip the
hook with `--no-hooks`. Helm then deletes the `ThoriumCluster` along with the rest of the release,
and with no operator to remove its finalizer it stays `Terminating`; as a last resort, remove the
finalizer by hand:

```bash
helm uninstall thorium -n thorium --no-hooks
kubectl -n thorium patch thoriumcluster thorium --type merge -p '{"metadata":{"finalizers":null}}'
```

Without the operator, nothing removes the node labels or provision pods, so clean those up too.
`operator.uninstallHook.enabled=false` leaves the hook out of the release on later installs and
upgrades; it doesn't affect an uninstall of a release that already has it.

Namespaces, volumes, and the `thorium-credentials` secret are kept after uninstalling; delete
the namespaces to remove the data. The group namespaces the k8s scaler created (labelled
`app.kubernetes.io/managed-by: thorium-scaler`) hold job data and are kept too; delete them with
`kubectl delete ns -l app.kubernetes.io/managed-by=thorium-scaler`. Group namespaces created by a
scaler that didn't label them must be deleted by hand.
