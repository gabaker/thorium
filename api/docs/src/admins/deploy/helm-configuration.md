# Configure the Helm Charts

This page explains how to configure the `thorium` and `infra-operators` charts: how the values
are organized, how credentials are generated and kept, how to shape the `ThoriumCluster` the
chart creates, how to use external backing services, and how to set up certificates, image pull
secrets, namespaces, ingress, proxies, and pod security. [Chart Values Reference](./chart-values.md)
lists every value with its default.

## Values structure

Each top-level key of the thorium chart's values configures one subchart:

| Key | Configures |
|-----|------------|
| `global` | Settings every subchart shares: `clusterDomain`, `clusterCIDRs`, `imageRegistry`, the `managed` service toggles, the `quickwit` settings, and `elasticIndices` |
| `secrets` | Namespaces and credentials (`thorium-credentials` and the Secrets derived from it) |
| `infra` | Redis, SeaweedFS, Scylla, Elasticsearch/Kibana, Quickwit's Postgres, Jaeger, an optional registry, and the setup jobs |
| `quickwit` | The vendored upstream Quickwit chart |
| `operator` | The operator, its RBAC, the `ThoriumCluster`, ingress, the toolbox import, and the uninstall hook |

Every chart validates its values against a schema, and values that don't exist fail rendering
instead of being ignored. A value that moved fails with a message naming the value to use, such
as `quickwit.enabled is not a chart value; use global.quickwit.enabled`. Backing services are
switched with `global.managed.<service>` only: `infra.<service>.enabled` and `secrets.consumers`
fail rendering with a message naming the toggle to use.

## Credentials

The `secrets` subchart keeps every credential in the `thorium-credentials` Secret in the thorium
namespace and renders each consumer's own Secret from it: Thorium's partial config
(`thorium-config-secrets`), the initial admin (`thorium-admin`), Redis's password, SeaweedFS's S3
identity, Elasticsearch's file-realm user and role, Quickwit's and Postgres's credentials, and the
optional registry's htpasswd.

| Value under `secrets.credentials` | Used for |
|-----------------------------------|----------|
| `thoriumSecretKey` | `thorium.secret_key`, which peppers every password hash |
| `redisPassword` | Redis |
| `scyllaUsername`, `scyllaPassword` | Thorium's Scylla role (username default `thorium`) |
| `elasticUsername`, `elasticPassword` | Thorium's Elasticsearch user (username default `thorium`) |
| `s3AccessKey`, `s3SecretToken` | Thorium's (and Quickwit's) S3 identity; stored in `thorium-credentials` under the key `s3SecretKey` |
| `postgresPassword` | The chart's Quickwit metastore Postgres (only with `global.managed.postgres` and `global.quickwit.enabled`) |
| `adminUsername`, `adminPassword` | The initial Thorium admin user (username default `admin`) |
| `registryPassword` | The optional registry's basic auth (with `secrets.registryBasicAuth.username`) |

A credential you leave empty is generated on the first install and read back with Helm's
`lookup` on every upgrade, so it never changes. A credential you set always wins over the stored
one, so setting one rotates it.

> **Never change `thoriumSecretKey` on an existing deployment.** Changing it locks out every
> user, including the operator's own. **Back up the `thorium-credentials` Secret** with the rest
> of your deployment (see [Backups](./operate.md#backups)).

Read the credentials with:

```bash
kubectl -n thorium get secret thorium-admin -o jsonpath='{.data.password}' | base64 -d; echo
kubectl -n thorium get secret thorium-credentials -o json | jq '.data | map_values(@base64d)'
```

Because a regenerated credential would lock out every user and break every backend, the chart
refuses to generate credentials when it can't read the old ones:

- **Without cluster access** (`helm template`, `helm install --dry-run` without `=server`, and
  GitOps renderers such as Argo CD or Flux), rendering fails unless every credential the
  deployment uses is set: `thoriumSecretKey`, `redisPassword`, `scyllaPassword`,
  `elasticPassword`, `s3AccessKey`, `s3SecretToken`, `adminPassword`, plus `postgresPassword` with
  the chart's Postgres and Quickwit, and `registryPassword` with registry basic auth. GitOps
  deployments supply them from their own secret store. Registry basic auth's `htpasswd` entry is
  a salted bcrypt hash, so it changes on every such render unless you also set
  `secrets.registryBasicAuth.htpasswd` to a precomputed line for the username and
  `registryPassword` (for example from `htpasswd -nbB <username> <password>`).
- **On `helm upgrade` with no `thorium-credentials` Secret**, rendering fails and asks you to
  restore it from your backup, unless every credential is set or `secrets.allowRegenerate: true`
  is set (only for a deployment with no data to keep).
- **External Redis and S3** issue their own credentials, so `redisPassword` (with
  `global.managed.redis: false`) and `s3AccessKey`/`s3SecretToken` (with `global.managed.s3: false`)
  are required.
- **`secrets.renderOnly: true`** renders placeholder credentials for output that is never
  applied, such as `helm lint`, `helm template` checks in CI, and `list-images.sh`. It fails
  during a real install or upgrade.

## ThoriumCluster settings

`operator.cluster` shapes the `ThoriumCluster` the chart creates.

| Value | Effect |
|-------|--------|
| `name` | The `ThoriumCluster`'s name (default `thorium`). Fixed after the first install: the chart refuses a different name next to the one it created |
| `version`, `imagePullPolicy` | The Thorium image tag and pull policy for the components (default `operator.image.tag`, which defaults to the chart's version, and `operator.image.pullPolicy`, default `Always`) |
| `components` | The components to deploy (see below) |
| `config` | Non-secret `thorium.yml` settings, deep-merged over the settings the chart generates from `operator.backends` (maps merge key by key; lists replace) |
| `configSecrets` | Secrets in the thorium namespace with partial `thorium.yml` documents the operator merges over `config` (see [Configuration and credentials](./concepts.md#configuration-and-credentials)). The first entry, `thorium-config-secrets`, holds every backend credential; Helm replaces lists, so keep it first when you add your own |
| `cors.insecure` | `thorium.cors.insecure` (default `true`, which lets any origin call the API). Set `false` in production and list allowed origins in `config.thorium.cors.domains`; the UI the API serves needs none |
| `crane.insecure` | `thorium.scaler.crane.insecure` (default `true`): the scaler reads tool image metadata over plain HTTP or unverified TLS. Set `false` once every tool image comes from a registry with a trusted certificate |
| `registryAuth` | Credentials for tool image registries, as `registry host: base64(user:password)`; the operator writes them to the `registry-token` pull secret and the scaler's `docker-skopeo` Secret |
| `namespaceBlacklist` | Extra namespaces Thorium must never use for a group (see [Namespaces](#namespaces)) |
| `bootstrap` | The initial admin user and the privileged Scylla and Elasticsearch setup (see [External services](#external-services)) |
| `upgrade` | `targetRevision`, `autoTargetDev`, and `approvals` (see [Upgrading Thorium](./upgrades.md)) |
| `scaler.k8s` | The k8s scaler's view of this cluster: `nodes` (the nodes Thorium may run jobs on; empty means every node), `alias` (the name Thorium shows), and `context` (the kube context name, `kubernetes-admin@cluster.local` for a scaler using its service account; change it only together with a kube config) |

The initial admin user comes from `bootstrap.admin.secret` (default the `thorium-admin` Secret the
chart renders, with keys `username` and `password`); the toolbox import logs in with the same
Secret. With `bootstrap.admin.enabled: false` no admin user is created, but the chart still renders
`thorium-admin`.

### Components

`operator.cluster.components` uses the `ThoriumCluster`'s snake_case component names: `api`,
`scaler`, `search_streamer`, `event_handler`, and `baremetal_scaler`. Set a component to `null` to
omit it (for example `scaler: null` to run Thorium without the k8s scaler) or to `{}` for the
`ThoriumCluster` defaults. Each component takes:

- `resources`: `cpu` in millicores and `memory` in MiB, applied as both request and limit; `0`
  leaves the container unbounded. The chart sets `0` for every component; without a value the
  `ThoriumCluster` defaults apply (2000 millicores and 8192 MiB for the API, 2000 millicores and
  4096 MiB for the others).
- `replicas` (API only; the chart sets 1, the `ThoriumCluster` default is 3).
- `env`, `cmd`, and `args`, each of which **replaces** the defaults. The default `env` sets empty
  proxy variables with `no_proxy=localhost,cluster.local`. The operator sets a scaler's `HOME`,
  and its `KUBECONFIG` when it uses the `kube-config` Secret, over any value in `env`.
- `service_account` (scaler only; the chart sets `true`): use the `thorium` service account
  instead of a kube config from the `kube-config` Secret (key `config`).

For example, to run the k8s scaler through a proxy (the scaler pulls image metadata from tool
registries):

```yaml
operator:
  cluster:
    components:
      scaler:
        service_account: true
        env:
          - { name: https_proxy, value: http://proxy.example.com:3128 }
          - { name: HTTPS_PROXY, value: http://proxy.example.com:3128 }
          - { name: no_proxy, value: "localhost,127.0.0.1,.svc,.svc.cluster.local,cluster.local,10.96.0.0/12,10.244.0.0/16" }
          - { name: NO_PROXY, value: "localhost,127.0.0.1,.svc,.svc.cluster.local,cluster.local,10.96.0.0/12,10.244.0.0/16" }
```

## External services

Every backing service can run outside the chart, such as a cloud service or one you already
operate. Set its `global.managed.<service>` toggle to `false`, point `operator.backends` at it,
and supply the credentials it issued under `secrets.credentials`. Rendering fails with a message
naming any missing value. [Bring Your Own Infrastructure](./infrastructure.md) describes setting
the services up yourself.

| Service | Toggle | When managed, the chart | When external, you provide | Either way |
|---------|--------|-------------------------|----------------------------|------------|
| Scylla | `global.managed.scylla` | Deploys a `ScyllaCluster`; the operator creates Thorium's role with the default `cassandra` superuser | `operator.backends.scylla.nodes` (and `replication`), and either Thorium's role with `scyllaUsername`/`scyllaPassword`, or the Scylla bootstrap with an admin Secret | The operator checks Thorium's login; the API creates its keyspace |
| Elasticsearch | `global.managed.elastic` | Deploys Elasticsearch and Kibana through ECK, with Thorium's user and role in ECK's file realm | `operator.backends.elastic.node`, optionally `caSecret`, and either Thorium's user with `elasticUsername`/`elasticPassword`, or the Elasticsearch bootstrap with an admin Secret | The operator checks Thorium's credentials and index privileges; the search streamer creates the indexes |
| Redis | `global.managed.redis` | Deploys Redis | `operator.backends.redis.host` (and `port`) and `secrets.credentials.redisPassword` | The operator checks `AUTH` and `PING` |
| S3 | `global.managed.s3` | Deploys SeaweedFS and, with Quickwit, creates the `quickwit` bucket | `operator.backends.s3.endpoint` (and `region`, `usePathStyle`), `s3AccessKey`/`s3SecretToken`, and with Quickwit `global.quickwit.s3Endpoint` and a `quickwit` bucket you created | The operator creates Thorium's buckets |
| Postgres | `global.managed.postgres` | Deploys Postgres for Quickwit's metastore and creates its `quickwit-metastore` database (only with Quickwit) | With Quickwit, `global.quickwit.metastoreUri` (such as `postgres://user:password@host:5432/quickwit-metastore`) for a database you created | Only Quickwit uses it |

For example, an external S3 store (such as a Rook/Ceph object store) instead of SeaweedFS:

```yaml
global:
  managed:
    s3: false
  quickwit:
    s3Endpoint: http://s3.example.com
secrets:
  credentials:
    s3AccessKey: <access key>
    s3SecretToken: <secret key>
operator:
  backends:
    s3:
      endpoint: http://s3.example.com
```

If Thorium's S3 identity may not create buckets, pre-create them and set
`operator.cluster.config.thorium.s3.skip_bucket_auto_create: true`. The operator then creates
nothing, but checks every required bucket on each reconcile and reports one error naming the
endpoint and every missing or inaccessible bucket.

### Thorium's Scylla role and Elasticsearch user

Thorium's own identities on an external Scylla or Elasticsearch must either exist already or be
created by the operator from an admin Secret you opt into. Admin Secrets are always read from the
thorium namespace.

- **Scylla**: create Thorium's role as a superuser (the API creates its keyspace) and set
  `secrets.credentials.scyllaUsername` and `scyllaPassword` to it, or set
  `operator.cluster.bootstrap.scylla.enabled: true` with `adminSecret` naming a Secret that holds
  superuser credentials (`{name, username or usernameKey, passwordKey}`). Without an admin Secret
  the bootstrap uses the default `cassandra`/`cassandra` superuser, which only exists on a fresh
  Scylla; `dropDefaultRole` (default `true`) drops it once Thorium's role exists.
- **Elasticsearch**: create Thorium's user with at least `view_index_metadata`, `write`, and
  `read` on every index in `global.elasticIndices`, plus `create_index` on any of them you don't
  pre-create (the search streamer creates them); `all` on `thorium*` covers the defaults, and the
  search streamer's `--reindex` also needs `delete_index`. Then set
  `secrets.credentials.elasticUsername` and `elasticPassword`, or set
  `operator.cluster.bootstrap.elastic.adminSecret.name` to a Secret holding superuser credentials;
  the operator then creates a role with `all` on `thorium*` (plus any configured index outside
  that pattern) and the user.

Without the matching bootstrap the chart refuses to render, since a generated password would match
no role or user you created: "external Scylla without bootstrap.scylla needs
secrets.credentials.scyllaUsername/scyllaPassword for the role you created", and the same for
Elasticsearch. Credentials already stored in `thorium-credentials` satisfy the check on later
upgrades. With a bootstrap enabled the password may stay empty and is generated, since the
operator creates the role or user with it.

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

The privileged bootstrap runs again only when `operator.cluster.bootstrap`, the Thorium config,
or a Secret it names changes. The admin Secrets may be deleted after setup: each step reads its
Secret only when it has work to do (Thorium's Scylla role can't log in, Thorium's Elasticsearch
user can't authenticate with the privileges it needs, or the admin user doesn't exist yet). If a
step needs a deleted Secret, the components still roll out (except without a working Scylla role)
and the `ThoriumCluster` reports `Error` naming the Secret until it exists again.

### What the operator checks

On every reconcile, before it writes `thorium.yml` or rolls out a component, the operator:

- creates Thorium's missing S3 buckets (existing buckets need no create rights);
- authenticates to Elasticsearch as Thorium's own user, checks which configured indexes exist
  (which needs `view_index_metadata`), and checks that it holds `view_index_metadata`, `write`, and
  `read` on every configured index plus `create_index` on each missing one;
- logs in to Scylla as Thorium's role and sends Redis `AUTH` and `PING` with Thorium's password.

A backend that isn't accepting connections yet leaves the phase at `Provisioning` (for example
"Waiting for Scylla to accept connections", or "Waiting for Elastic to accept Thorium's
credentials" while ECK applies Thorium's file-realm user); a rejected credential is an `Error`.

## Elasticsearch certificates

The chart's own Elasticsearch serves a self-signed ECK certificate whose CA lives in the elastic
namespace, which the operator can't read, so Thorium reaches it without verifying the certificate
(`operator.backends.elastic.insecureCertificates: true`); that traffic never leaves the cluster.

To verify an external Elasticsearch, put the PEM CA that signed its certificate in a Secret in the
thorium namespace and point `caSecret` at it:

```bash
kubectl -n thorium create secret generic elastic-ca --from-file=ca.crt=./elastic-ca.pem
```

```yaml
global:
  managed:
    elastic: false
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
operator mounts it in every Thorium component (`spec.elastic_ca_secret`). The rendered config then
uses `cert_validation: {Full: /etc/thorium/elastic-ca/ca.crt}` with `insecure_certificates:
false`, so the certificate chain and the hostname in `node` are verified (`insecureCertificates` is
ignored). `caSecret` is only accepted with `global.managed.elastic: false`. Without it an external
Elasticsearch keeps `insecureCertificates` (unverified by default), and the install notes print a
warning.

Components read the CA when they start, and the operator rolls them out when its content changes.
After you rotate the CA Secret the operator reads the new CA from its own mounted file, which the
kubelet refreshes with a delay (typically up to about a minute); until then the status may briefly
show an Elasticsearch TLS error, which clears on its own.

## Image pull secrets

| Value | Effect |
|-------|--------|
| `operator.createImagePullSecret` with `operator.dockerConfigJson` | Renders the `thorium-image-pull` Secret from the contents of a `.dockerconfigjson` file (for example `--set-file operator.dockerConfigJson=$HOME/.docker/config.json`) |
| `operator.imagePullSecrets` | A list of existing `kubernetes.io/dockerconfigjson` Secrets in the thorium namespace, by name |
| `operator.cluster.registryAuth` | Credentials for tool image registries, written by the operator to its own `registry-token` Secret |

The operator, the chart's jobs, and (through the `ThoriumCluster`'s `image_pull_secrets`) every
Thorium component and node provision pod reference the first two. While `registryAuth` is set,
every Thorium pod also references `registry-token`, so `operator.imagePullSecrets` can't list
`registry-token`: the operator would overwrite it.

Config and bootstrap Secrets can't use a name the operator manages: `thorium`, `keys`,
`keys-kaboom`, `docker-skopeo`, `registry-token`, `thorium-image-pull`, or any name ending in
`-pass`.

## Namespaces

`secrets.createNamespaces` (default `true`) creates the deployment's namespaces (see
[Namespaces and the namespace prefix](./concepts.md#namespaces-and-the-namespace-prefix)) except
the release namespace, which `helm install --create-namespace` creates. Namespaces of services the
chart doesn't deploy are skipped. With `false`, create every namespace the deployment uses
yourself before installing.

The chart renders `thorium.namespace_blacklist`, the namespaces Thorium never creates or uses for
a group: every namespace of this deployment with its prefix, `infra-operators`, `kube-system`,
`kube-public`, `kube-node-lease`, `default`, and Thorium's built-in defaults (`thorium`, `scylla`,
`scylla-operator`, `cert-manager`, `redis`, `elastic-system`, `jaeger`, `quickwit`). Add names with
`operator.cluster.namespaceBlacklist`, or replace the whole list with
`operator.cluster.config.thorium.namespace_blacklist`.

`operator.controller.watchOwnNamespaceOnly` (default `true`) is a security scope: the operator
only reconciles `ThoriumCluster`s in the thorium namespace, and its permissions on
`ThoriumCluster`s, Secrets, pods, Services, ConfigMaps, and Deployments are bound in that
namespace instead of granted cluster-wide. Turn it off only if the operator must manage a
`ThoriumCluster` in another namespace; it still manages only one.

## Ingress

`operator.ingress` exposes the Thorium API and UI (Service `thorium-api`, port 80):

| Value | Effect |
|-------|--------|
| `type` | `none` (default), `nginx` (a `networking.k8s.io` Ingress), or `traefik` (an `IngressRoute` on the `websecure` entrypoint plus a `TLSOption` limiting TLS to 1.2 and 1.3) |
| `host` | The hostname to route; empty routes every host |
| `className` | The Ingress class (nginx only, default `nginx`) |
| `annotations` | Extra Ingress annotations (nginx only), merged over the chart's `nginx.ingress.kubernetes.io/proxy-body-size: "0"` (and `ssl-redirect: "false"` without TLS) |
| `tls.secretName` | An existing `kubernetes.io/tls` Secret in the thorium namespace. With nginx and no Secret, the Ingress serves plain HTTP |
| `tls.traefikDefaultStore` | Make `secretName` Traefik's cluster-wide default certificate (the `default` TLSStore, default `true`); Traefik allows one default certificate per cluster (traefik only) |
| `registryHost` | Also route a hostname to the chart's registry, which needs `infra.registry.enabled` (traefik only) |

Create the TLS Secret before installing:

```bash
kubectl -n thorium create secret tls thorium-tls --cert=thorium.crt --key=thorium.key
```

## Proxies

`operator.controller.proxy` (`httpProxy`, `httpsProxy`, `noProxy`) sets the operator's proxy
environment. When a proxy is set, the operator's `no_proxy`/`NO_PROXY` is the cluster's own names
and ranges followed by your `noProxy` entries:

- `localhost`, `127.0.0.1`, `.svc`, `.svc.<clusterDomain>`, and `<clusterDomain>`
  (`global.clusterDomain`, default `cluster.local`)
- every CIDR in `global.clusterCIDRs` (default empty)

Set `global.clusterCIDRs` to the cluster's service and pod CIDRs, which the chart can't detect;
the install notes warn when a proxy is set without them. Traffic addressed by IP (such as calls to
the Kubernetes API by its Service IP) otherwise matches none of the names above and is sent
through the proxy:

```yaml
global:
  # the kubeadm/minikube defaults; use your cluster's service and pod CIDRs
  clusterCIDRs: [10.96.0.0/12, 10.244.0.0/16]
operator:
  controller:
    proxy:
      httpsProxy: http://proxy.example.com:3128
      # appended after the defaults above
      noProxy: .corp.example.com
```

The Thorium components take their proxy settings from their own `env` in
`operator.cluster.components` (see [Components](#components)), which the chart doesn't fill in.

## Pod security

| Pods | Defaults | Values |
|------|----------|--------|
| Operator | Restricted: the Thorium image's `thorium` user (uid and gid 10001) with `runAsNonRoot`, `RuntimeDefault` seccomp, no privilege escalation, every capability dropped | `operator.controller.securityContext`, `operator.controller.podSecurityContext` |
| Uninstall hook and toolbox import Jobs | Restricted: user and group 65534, `RuntimeDefault` seccomp, no privilege escalation, every capability dropped, read-only root filesystem with an `emptyDir` at `/tmp` | `operator.uninstallHook.*`, `operator.toolbox.*` (`podSecurityContext`, `securityContext`, `resources`) |
| Redis, SeaweedFS, Jaeger, the registry, and the setup jobs | Restricted: each runs as its image's own non-root user, with a read-only root filesystem, no privilege escalation, and every capability dropped | `infra.<service>.podSecurityContext`, `infra.<service>.securityContext`, `infra.jobs.*` |
| Scylla, Elasticsearch, Kibana, Postgres | Set by their operators | Scylla's core dump `hostPath` (`infra.scylla.coredumps`, default on) isn't allowed by the `baseline` or `restricted` levels; `infra.elastic.setMaxMapCount` adds a privileged init container |
| Thorium components (API, scalers, event handler, search streamer) | Restricted, set by the Thorium operator: user and group 10001 with `runAsNonRoot`, `RuntimeDefault` seccomp, no privilege escalation, every capability dropped. The API pod sets the namespaced sysctl `net.ipv4.ip_unprivileged_port_start=0` (allowed at every level) so it can listen on port 80 | |
| Node provision pods | Set by the Thorium operator: run as root and mount the node's `/opt` to install the agent, which only the `privileged` level allows | |

Set a key to `null` to drop one of these defaults. The operator writes nothing to disk, so it
also runs with `operator.controller.securityContext.readOnlyRootFilesystem: true`.

## Rollouts

The operator annotates each component's pod template with the hash of the rendered `thorium.yml`
(`thorium.sandia.gov/config-hash`) and of everything else it mounts and reads only at startup
(`thorium.sandia.gov/mounts-hash`: the `keys` Secret, the `banner` ConfigMap for the API, the
Elasticsearch CA, and the `kube-config` Secret for a scaler without a service account). Components
roll out when the config, a mounted input, or their own spec changes, and are left alone
otherwise. The operator watches every Secret a `ThoriumCluster` names and the `banner` ConfigMap,
and reconciles when one changes.

A new image pushed under the same tag changes none of these, so restart the components yourself:

```bash
kubectl -n thorium rollout restart deployment
```

Node provision pods are annotated with the Thorium version the API reports
(`thorium.sandia.gov/thorium-version`) and the hash of their template
(`thorium.sandia.gov/pod-hash`). Once the API reports a new version, the operator's node watcher
(which checks each node every 15 minutes) or the next reconcile replaces them, which updates the
agents. After a same-tag push that keeps the reported version, delete them and restart the
operator to reinstall the agent; nothing watches provision pods, so they come back on the
reconcile the operator runs when it starts:

```bash
kubectl -n thorium delete pod -l app=node-provisioner
kubectl -n thorium rollout restart deployment/operator
```

## Setup jobs

The chart's setup jobs, which create Quickwit's metastore database and SeaweedFS buckets
(`infra.jobs.*`) and import the toolbox (`operator.toolbox.*`), are ordinary resources rather than
Helm hooks, so `helm install --wait`, Argo CD, and Flux all converge: the jobs start alongside the
services they need and retry until those are reachable, and Quickwit restarts until its database
and bucket exist. Each job is named after a hash of its rendered spec, since a Job's pod template
can't be changed: any change to it (a new image tag, other scheduling or pull settings, new
toolbox settings) replaces the job with a new one, which Helm (or your GitOps tool) creates while
deleting the old one, and every job is safe to repeat. Completed jobs are kept until the next
change replaces them. Releases from charts that ran the database and bucket jobs as hooks leave
the last hook runs behind, `quickwit-create-metastore-db` in the `quickwit` namespace and
`seaweedfs-create-buckets` in `seaweedfs` (with the namespace prefix, if any); delete them once
the upgrade is done.

## The infra-operators chart

| Value | Effect |
|-------|--------|
| `scylla-operator.enabled`, `eck-operator.enabled`, `kubegres.enabled` | Install each operator (default `true`); turn off any operator the cluster already runs |
| `global.imageRegistry` | Pull every operator image from a mirror registry, and render the Scylla operator's `ScyllaOperatorConfig` so its auxiliary images come from it too (see below) |
| `global.clusterDomain` | The cluster's DNS domain (default `cluster.local`), used in the SANs of the generated Scylla webhook certificate |
| `scylla-operator.webhook.createSelfSignedCertificate` | Generate the Scylla operator's webhook certificate in the chart (default `true`, valid 10 years), so cert-manager isn't needed |
| `scylla-operator.webhook.tls.caCrt`, `.crt`, `.key` | A fixed webhook certificate (PEM) instead of a generated one, for GitOps renderers (see below) |
| `scyllaOperatorConfig.create`, `.scyllaDBUtilsImage`, `.bashToolsImage` | Render the `ScyllaOperatorConfig` without a mirror too, and the auxiliary images it names |

The generated webhook certificate is stored in the `scylla-operator-serving-cert` Secret and read
back on upgrades. Renderers that can't read the cluster (`helm template`, Argo CD, Flux) would
generate a new CA on every render, so give them a fixed certificate in
`scylla-operator.webhook.tls`: `caCrt`, `crt`, and `key` together, with `crt` valid for
`infra-operators-scylla-operator-webhook.infra-operators.svc` (`<release>-scylla-operator-webhook.<namespace>.svc`).
To manage the Secret yourself instead, set `scylla-operator.webhook.createSelfSignedCertificate:
false`, `scylla-operator.webhook.certificateSecretName` to your Secret, and
`scylla-operator.webhook.tls.caCrt` to its CA.

The Scylla operator reads cluster-wide settings from a `ScyllaOperatorConfig` named `cluster`,
creating an empty one when none exists, and otherwise uses the upstream ScyllaDB utilities and
Bash tools images built into it (it runs them only for `NodeConfig` node tuning and
`ScyllaDBMonitoring`, which Thorium doesn't create). The chart renders `cluster` only when
`global.imageRegistry` is set (or `scyllaOperatorConfig.create` is true), naming those images
moved to the mirror; without it the operator's own defaults, which follow operator upgrades, are
left alone. A release first installed without a mirror already has the operator's `cluster`, so
pass `--take-ownership` to the `helm upgrade` that sets `global.imageRegistry` for Helm to adopt
it.

Helm installs the CRDs in the chart's `crds/` directory (Scylla and Kubegres) only on the first
install and never upgrades them, so apply the new CRDs by hand when upgrading to a chart that
vendors newer operators. ECK's CRDs are templates and are upgraded with the chart.
