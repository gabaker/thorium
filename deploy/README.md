# Thorium Helm Charts

This folder holds the Helm charts that deploy Thorium and its backing services onto any
Kubernetes cluster. `minithor/` (single-node development) and `megathor/` (multi-node
clusters) both install these charts, and they can be installed directly with Helm. The
user-facing guide is the mdbook page `api/docs/src/admins/deploy/deploy-helm.md`.

## Layout

| Path | Purpose |
|------|---------|
| `charts/infra-operators` | Cluster-wide operators: Scylla, Elastic Cloud on Kubernetes (ECK), Kubegres. Installed once per cluster |
| `charts/thorium` | Thorium itself (once per cluster): the umbrella chart over the `secrets`, `infra`, `quickwit`, and `operator` subcharts in `charts/thorium/charts/` |
| `charts/thorium/values-production.yaml` | A multi-node starting point layered over the chart defaults |
| `charts/scripts/vendor-charts.sh` | Re-vendors the upstream charts (Quickwit, Scylla operator, ECK operator, Kubegres) with their patches |
| `charts/scripts/list-images.sh` | Prints every image the charts deploy, with its path in a mirror registry |
| `charts/scripts/package.sh` | Packages both charts, optionally pointing the thorium chart at a given Thorium image; used by the Helm Charts workflow and for local testing |

Both charts carry all of their subcharts, so they install from a directory or packaged `.tgz`
with no dependency downloads. `.github/workflows/charts.yml` publishes them to
`oci://ghcr.io/<owner>/<repo>/charts` (`oci://ghcr.io/cisagov/thorium/charts` for the core repo)
under the same rules as the Thorium image (`.github/scripts/release_refs.py`): prereleases
(`<chart version>-<branch>.<run number>.g<short sha>`) from `main` on the core repo and from
every branch on forks, releases from `X.Y.Z` release tags (equal to the chart version or a prerelease of it),
and nothing from pull requests. A published thorium chart deploys the image `deploy.yml`
publishes for the same branch or tag in the same repository, so a fork's charts run that fork's
image. The two workflows run independently, so a chart can be published before its image is, or
without one when the image build fails.

## How the thorium chart fits together

| Subchart | Contents |
|----------|----------|
| `secrets` | Thorium's namespaces and one canonical `thorium-credentials` secret. Credentials not supplied are generated on first install and read back (Helm `lookup`) on every upgrade, then rendered into each consumer's own secret format (Redis password, SeaweedFS S3 identities, Quickwit/Postgres credentials, the admin user, a partial `thorium.yml`, the optional registry's htpasswd) |
| `infra` | Redis, SeaweedFS (`weed mini`), the `ScyllaCluster`, Elasticsearch/Kibana, Kubegres Postgres for Quickwit's metastore (each only when `global.managed` keeps it), Jaeger, an optional registry, and jobs that create the Quickwit bucket and metastore database in the chart's SeaweedFS and Postgres |
| `quickwit` | Upstream Quickwit chart, vendored and patched to install into `<prefix>-quickwit` and honor `global.imageRegistry` |
| `operator` | The namespace-scoped Thorium operator, its RBAC and CRD, the `ThoriumCluster` (no secrets in `spec.config`; credentials arrive through `config_secrets` and `bootstrap`), ingress (nginx or Traefik), and an optional toolbox import job |

The operator merges the `config_secrets` over `spec.config`, creates Thorium's buckets and Scylla
role, checks that Elasticsearch accepts Thorium's own credentials with the index privileges the
API and search streamer need and that they log in to Scylla and Redis, and only then writes the rendered `thorium.yml` to the `thorium` Secret, so a bad credential is
reported in the status instead of being rolled out to the components. It then deploys the
Thorium components, registers and provisions the scaler's nodes (only when
`components.scaler` is set), creates the initial admin user, and reports progress in
`status.phase` (`Provisioning`, `Ready`, or `Error`, with `status.observed_generation` naming
the spec generation it describes). `Ready` means every component Deployment finished rolling
out (all replicas updated and available). While components roll out the phase is
`Provisioning` with "Waiting for <deployments> to roll out", and `status.message` adds the
waiting reason or last termination of any failing container (a crash loop, an image pull or
container creation error, or a non-zero exit). A reconcile only waits about 20 seconds for a
rollout to finish and then requeues itself to check again 15 seconds later, so a spec change made
while a rollout is stuck is picked up within about 20 seconds; a rollout past its progress
deadline sets the phase to `Error`. Non-fatal problems (such as an index whose `group` field isn't a keyword)
are noted in `status.message` alongside `Ready`.

The phases mean:

| Phase | Meaning |
|-------|---------|
| `Provisioning` | the operator is deploying, or waiting for a backend or rollout that will finish on its own: a backend that isn't reachable or accepting connections yet (Scylla, Redis, S3, or Elasticsearch, including "Waiting for Elastic to accept Thorium's credentials" while ECK applies its file realm), or Deployments still rolling out. `status.message` says what it is waiting on (with the underlying error) and the operator checks again every 15 seconds |
| `Ready` | every backend check passed and every component Deployment finished rolling out |
| `Error` | something needs attention and won't fix itself: credentials a reachable backend rejected, an invalid config or a missing config Secret, required buckets missing with `skip_bucket_auto_create`, missing Elastic privileges, a rollout past its progress deadline, a missing bootstrap admin Secret that is needed ("bootstrap is incomplete"), or a Kubernetes API permission error |

A fresh install normally passes through `Provisioning` for several minutes while the backends
start; only `Error` needs action. An Elasticsearch that refuses every connection is waited on, but
a certificate that fails validation is an `Error`. The search streamer creates Thorium's Elastic
indexes (with explicit mappings) when it starts and streams every existing item into a newly
created index. The chart's Elasticsearch gets Thorium's user and role from ECK's file
realm (the `thorium-elastic-user` and `thorium-elastic-roles` secrets in `<prefix>-elastic`), so
the operator never needs the `elastic` superuser.

`global.managed.scylla`, `global.managed.elastic`, `global.managed.redis`, `global.managed.s3`
(SeaweedFS), and `global.managed.postgres` (Quickwit's metastore) choose whether the chart deploys
each backing service; all default to `true`. Every subchart follows them: `infra` deploys the
service, `secrets` renders its credentials (and skips its otherwise-empty namespace when it is
off), and the operator subchart uses the in-cluster endpoint only for a managed service. The
Scylla and Elasticsearch toggles also pick the matching `operator.cluster.bootstrap` defaults
(each bootstrap toggle can still be set explicitly). See "External services".

### Config secrets

Each `config_secrets` entry is a partial `thorium.yml` merged over `spec.config` in order as a
JSON merge patch (RFC 7386): objects merge key by key, lists and scalars replace the earlier
value, and a `null` deletes the key. The merged result must deserialize into Thorium's config
types, which differs from how a `thorium.yml` file is loaded:

- YAML tags (such as `!Grpc`), non-string map keys, and merge keys (`<<`) are not supported.
- Values are not coerced: a quoted `"5"` stays a string, so every value must already have the
  type Thorium expects.

### Rollouts

The operator annotates every component's pod template with the hash of the rendered
`thorium.yml` (`thorium.sandia.gov/config-hash`) and the hash of everything else it mounts and
only reads at startup (`thorium.sandia.gov/mounts-hash`): the `keys` Secret for the scaler, event
handler, and search streamer, the `banner` ConfigMap for the API, the Elastic CA Secret
(`spec.elastic_ca_secret`) for every component, and the `kube-config` Secret for a k8s scaler
without a service account. Every other spec field a component depends on is part of its pod
template already, so components roll out when the config, a mounted input, or their own spec
changes and are left alone otherwise. Node provision pods are annotated with the Thorium version
the API reports (`thorium.sandia.gov/thorium-version`) and the hash of their template
(`thorium.sandia.gov/pod-hash`), so they are replaced when their image, their spec, or the
reported version changes.

Pushing a new image under the same tag changes none of the component pod templates, so restart
the components yourself to pick it up:

```bash
kubectl -n thorium rollout restart deployment
```

Once the restarted API reports a new version, the operator's node watcher (which checks each
node every 15 minutes) or the next `ThoriumCluster` reconcile replaces the provision pods and
relabels the nodes, so agents are updated without further steps. A same-tag push that keeps the reported version leaves the provision pods in place; delete
them to reinstall the agent from the new image:

```bash
kubectl -n thorium delete pod -l app=node-provisioner
```

The privileged bootstrap steps (Scylla role, external Elastic role and user, and the initial
admin user) run only when `spec.bootstrap`, the rendered config, or a Secret the bootstrap
names changes; `status.bootstrap_hash` records the inputs of the last completed run. The
operator watches every Secret a `ThoriumCluster` names (`config_secrets`, `bootstrap`, and
`elastic_ca_secret`), the `kube-config` Secret when the scaler doesn't use a service account, and
the `banner` ConfigMap, and reconciles when one changes.

The bootstrap admin Secrets may be deleted once setup is done. A deleted Secret is hashed as an
`<absent:name>` marker, and each step only reads its Secret when it has work to do: the Scylla
admin Secret only when Thorium's role can't log in, the Elastic admin Secret only when Thorium's
own Elastic user can't authenticate with the privileges it needs, and the `thorium-admin` Secret
only when the admin user doesn't exist yet. When a step does need a deleted Secret, the
components still roll out with the current config (except for a missing Scylla role, which the
components can't run without), and then the `ThoriumCluster` reports `Error` naming the missing
Secret and the bootstrap is retried.

## External services

Every backing service can be managed by the chart or run externally (for example a cloud
service). Set its `global.managed.<service>` toggle to `false` and supply the external settings;
rendering fails with a message naming any missing value. `infra.redis.enabled`,
`infra.seaweedfs.enabled`, `infra.postgres.enabled`, `infra.scylla.enabled`,
`infra.elastic.enabled`, and `secrets.consumers.*` are not chart values; rendering fails when one
is set, naming the `global.managed` toggle to use instead.

| Service | Toggle | When managed, the chart | When external, the site provides | Runs automatically either way |
|---------|--------|-------------------------|----------------------------------|-------------------------------|
| Scylla | `global.managed.scylla` | deploys a `ScyllaCluster`; the operator creates Thorium's role with the default `cassandra` superuser (`bootstrap.scylla`) | `operator.backends.scylla.nodes`, and either Thorium's role pre-created (a superuser, since the API creates its keyspace) with `secrets.credentials.scyllaUsername`/`scyllaPassword` set to that role (enforced at render time), or `operator.cluster.bootstrap.scylla.enabled: true` with an `adminSecret` holding superuser credentials | the operator logs in as Thorium's role; the API creates its keyspace |
| Elasticsearch | `global.managed.elastic` | deploys Elasticsearch/Kibana through ECK, with Thorium's user and role in ECK's file realm | `operator.backends.elastic.node`, optionally `operator.backends.elastic.caSecret`, and either Thorium's user pre-created (see below) with `secrets.credentials.elasticUsername`/`elasticPassword` set to that user (enforced at render time), or `operator.cluster.bootstrap.elastic.adminSecret.name` set to a Secret with superuser credentials | the operator verifies Thorium's credentials and its `view_index_metadata`, `write`, and `read` privileges on every index (plus `create_index` on indexes that don't exist yet); the search streamer creates the indexes |
| Redis | `global.managed.redis` | deploys Redis with the generated password | `operator.backends.redis.host` (and `.port`) and `secrets.credentials.redisPassword`, the password that Redis requires | the operator sends `AUTH` and `PING` with Thorium's credentials |
| S3 | `global.managed.s3` | deploys SeaweedFS with a generated Thorium identity and creates the `quickwit` bucket | `operator.backends.s3.endpoint` (and `.region`, `.usePathStyle`), `secrets.credentials.s3AccessKey`/`s3SecretKey`, and with Quickwit `secrets.quickwitS3Endpoint` and a `quickwit` bucket the site created | the operator creates Thorium's missing buckets, or with `thorium.s3.skip_bucket_auto_create` only checks that they all exist |
| Postgres | `global.managed.postgres` | deploys Kubegres Postgres for Quickwit's metastore and creates the `quickwit-metastore` database | with Quickwit, `secrets.quickwitMetastoreUri` pointing at a database the site created | nothing (only Quickwit uses it) |

What the operator does on every reconcile, with the credentials in the merged config, before it
writes `thorium.yml` or rolls out any component:

- creates Thorium's S3 buckets that don't exist yet (a `HeadBucket` first, so pre-created buckets
  need no create rights). With `thorium.s3.skip_bucket_auto_create` it creates nothing but still
  sends a `HeadBucket` for every required bucket (files, repos, attachments, results, ephemeral,
  graphics, reaction cache) and fails with one error naming the S3 endpoint and every missing or
  inaccessible bucket, with the reason for each
- authenticates to Elasticsearch as Thorium's own user (`_security/_authenticate`), checks which
  configured indexes already exist (`HEAD /<index>`, which needs `view_index_metadata`; a 403 is
  reported as that missing privilege), and checks with `_security/user/_has_privileges` that it
  holds `view_index_metadata`, `write`, and `read` on every configured index plus `create_index`
  on each index that doesn't exist yet. The operator creates no indexes: the search
  streamer creates them with their mappings and streams every existing item into them. With the
  chart's Elasticsearch, the phase stays `Provisioning` with "Waiting for Elastic to accept
  Thorium's credentials" until ECK has applied the file-realm user, and is checked again
- logs in to Scylla as Thorium's own role and sends Redis `AUTH` and `PING` with Thorium's
  credentials

The privileged bootstrap steps (`bootstrap.scylla`, `bootstrap.elastic`) run before these
checks. Admin Secrets are read only from the thorium namespace (`thorium` or `<prefix>-thorium`,
the `ThoriumCluster`'s own namespace; the operator reads no Secrets anywhere else).

Without the opt-in bootstrap, nothing creates Thorium's Scylla role or Elastic user, and the
generated `scyllaPassword`/`elasticPassword` match nothing in an external service. Set
`secrets.credentials.scyllaUsername` and `scyllaPassword` to the role the site created, and
`secrets.credentials.elasticUsername` and `elasticPassword` to its Elastic user (both usernames
default to `thorium`). The chart enforces this: with `global.managed.scylla: false` and the Scylla
bootstrap off (`bootstrap.scylla.enabled` unset or `false`), rendering fails with "external
Scylla without bootstrap.scylla needs secrets.credentials.scyllaUsername/scyllaPassword for the
role you created" unless both are set, and likewise for an external Elasticsearch without the
Elastic bootstrap (`bootstrap.elastic` on only with `adminSecret.name`). A credential already
stored in `thorium-credentials` (a later `helm upgrade`) also satisfies it, and so do the
`secrets.renderOnly` placeholders: render-only tooling has no cluster and never installs, while
every real install and upgrade enforces the check. With either bootstrap enabled the password may still be generated,
since the operator creates the role or user with it. The operator's checks report a rejected
login in `status.message`.

When Scylla rejects the default `cassandra` login and no `adminSecret` is set, the operator
reports in `status.message` that a `bootstrap.scylla.admin_secret` is required. A Scylla that
isn't accepting connections yet is reported as "Waiting for Scylla to accept connections" with
the driver's error, with the phase left at `Provisioning`, and checked again 15 seconds later.

A pre-created Elastic user needs at least `view_index_metadata`, `write`, and `read` on every
configured index (`global.elasticIndices`): the search streamer checks for each index and writes
documents into it, and the API searches it. If the indexes aren't pre-created it also needs
`create_index` on them, since the search streamer creates them; once every index exists,
`create_index` is no longer checked. The chart's file-realm role and
`bootstrap.elastic` grant `all` on `thorium*` plus any configured index outside that pattern.
Running the search streamer with `--reindex` also needs `delete_index`.

Elasticsearch TLS: the chart-managed Elasticsearch is reached without certificate verification
(`insecureCertificates: true`). ECK serves a self-signed certificate whose CA lives in
`<prefix>-elastic`, which the operator can't read, and that traffic stays inside the cluster. For
an external Elasticsearch, put the CA that signed its certificate in a Secret in the thorium
namespace and set `operator.backends.elastic.caSecret.name` (and `.key`, default `ca.crt`). The
chart then mounts the CA read-only at `/etc/thorium/elastic-ca/ca.crt` in the operator, and the
operator mounts it in every Thorium component (`spec.elastic_ca_secret`). The rendered thorium.yml
then uses `cert_validation: {Full: /etc/thorium/elastic-ca/ca.crt}` with `insecure_certificates:
false`, which verifies the chain and the hostname in `operator.backends.elastic.node`. Without a
CA Secret an external Elasticsearch keeps `insecureCertificates` (unverified by default), and the
install notes print a warning. The API, operator, and search streamer all validate Elastic's
certificate the same way. Components read the CA when they start; the operator hashes its
content into their pod templates, so rotating it rolls them out.

After rotating the Elastic CA Secret, the operator itself reads the CA from its mounted file,
which the kubelet refreshes with a delay (typically up to about a minute). Until then the
`ThoriumCluster` status may briefly show an Elastic TLS error, which clears on its own once the
mounted file is updated; the components roll out with the new CA through the mounts hash.

The Quickwit bucket and the Postgres metastore database are created by `infra` jobs only for the
chart-managed SeaweedFS and Postgres. Sites using an external S3 store or Postgres create them
before installing; the operator never creates them.

Config and bootstrap Secrets can't reuse a name the operator manages (`thorium`, `keys`,
`keys-kaboom`, `docker-skopeo`, `registry-token`, `thorium-image-pull`, or any `*-pass`); the
`ThoriumCluster` goes to `Error` with a message naming the collision.

### Who may edit a ThoriumCluster

Anyone who can create or edit a `ThoriumCluster` controls the images, commands, arguments, and
environment of every component the operator deploys, and the endpoints it sends Secret-backed
credentials to. The k8s scaler runs with cluster-wide Secret read, and node provision pods mount
host paths as root. Edit rights on a `ThoriumCluster` are therefore roughly equivalent to
cluster-wide Secret read plus root on the nodes: grant them like `cluster-admin`.

### Credentials that must not change or be lost

- `secrets.credentials.thoriumSecretKey` (the config's `thorium.secret_key`) peppers every user's
  password hash. Never change it on an existing deployment: every user, including the operator's
  own, would be locked out.
- Back up the `thorium-credentials` Secret. Credentials left empty are generated once and read
  back with Helm `lookup` on every upgrade. New values would break every backend still using the
  old ones and, for `thoriumSecretKey`, lock out every user, so the `secrets` subchart refuses to
  generate them when it can't read the old ones:
  - When `lookup` can't reach the cluster (`helm template`, client-side `--dry-run`, GitOps
    renderers such as Argo CD or Flux), rendering fails unless every credential the deployment
    uses is set under `secrets.credentials`: `thoriumSecretKey`, `redisPassword`,
    `scyllaPassword`, `elasticPassword`, `s3AccessKey`, `s3SecretKey`, `adminPassword`, plus
    `postgresPassword` with `global.managed.postgres` and `registryPassword` with
    `secrets.registryAuth.username`. The chart detects this by looking up the `kube-system`
    Namespace, which exists on every cluster, so an empty result can only mean `lookup` is
    unavailable. GitOps deployments should supply these from their own secret store.
  - On `helm upgrade` with no `thorium-credentials` Secret, rendering fails and asks you to
    restore it from your backup, unless every credential above is set or
    `secrets.allowRegenerate: true` is set (only for a deployment with no data to keep).
  - `secrets.renderOnly: true` renders placeholder credentials (`render-only-<name>`) for output
    that is never applied, such as CI's `helm lint` and `helm template` checks and
    `charts/scripts/list-images.sh`. It fails whenever `lookup` works, so it can't affect a real
    install or upgrade. `helm lint` can't use `lookup` either, so lint the thorium chart with it
    (otherwise lint stops at the credentials guard and checks none of the templates):
    `helm lint --strict -n thorium --set secrets.renderOnly=true charts/thorium`.

### Image pull secrets

`operator.imagePullSecret.create` renders the `thorium-image-pull` Secret (or set
`operator.imagePullSecret.name` to reference an existing one). The operator, the chart's jobs,
and, through the `ThoriumCluster`'s `image_pull_secrets`, every Thorium component and node
provision pod reference it. The operator renders its own `registry-token` Secret from
`operator.cluster.registryAuth` (tool registries), which every Thorium pod also references when
set, so don't point `operator.imagePullSecret.name` at `registry-token` while `registryAuth` is
set: the operator would overwrite it.

## Namespaces and the namespace prefix

A cluster runs one Thorium deployment. It is installed into `thorium` or `<prefix>-thorium`
(e.g. `dev-thorium`), where the optional namespace prefix is a lowercase DNS label of at most 32
characters (megathor's `namespace_prefix`, minithor's `--namespace-prefix`). Every chart derives
the prefix from the release namespace and uses it for all of Thorium's namespaces
(`<prefix>-redis`, `<prefix>-scylla`, `<prefix>-elastic`, `<prefix>-seaweedfs`,
`<prefix>-quickwit`, `<prefix>-jaeger`). Install with `--create-namespace`; the `secrets`
subchart creates the rest.

The operator subchart renders `thorium.namespace_blacklist`, the namespaces Thorium never creates
or uses for a group, from every one of those namespaces with the prefix applied, plus
`infra-operators`, `kube-system`, `kube-public`, `kube-node-lease`, `default`, and Thorium's
built-in defaults (`thorium`, `scylla`, `scylla-operator`, `cert-manager`, `redis`,
`elastic-system`, `jaeger`, `quickwit`). Add names with `operator.cluster.namespaceBlacklist`, or
replace the whole list with `operator.cluster.config.thorium.namespace_blacklist`. CI renders
`-n dev-thorium` and checks the list; to check it by hand:

```bash
helm template thorium charts/thorium -n dev-thorium --set secrets.renderOnly=true \
  --show-only charts/operator/templates/thoriumcluster.yaml | grep -A24 namespace_blacklist
```

The k8s scaler creates a namespace for each Thorium group, labelled
`app.kubernetes.io/managed-by: thorium-scaler`. Deleting a `ThoriumCluster` or uninstalling the
chart leaves them and the job data in them; `minithor cleanup --confirm` deletes them. Group
namespaces created by a scaler that didn't label them must be deleted by hand.

`operator.operator.watchOwnNamespaceOnly` (default `true`) is a security scope: the Thorium
operator only reconciles `ThoriumCluster`s in the thorium namespace, and its permissions on
Secrets, pods, services, ConfigMaps, and deployments are bound there rather than granted
cluster-wide. The operator Deployment uses the `Recreate` strategy and has no leader election.

## Moving from a native-realm Elasticsearch user

Thorium's Elastic user comes from ECK's file realm (chart-managed Elasticsearch) or from a
pre-created identity, and its indexes are created with explicit mappings (`group` as a
`keyword`). On a deployment whose Elastic user was created in the native realm and whose
indexes were created from dynamic mappings:

- Remove the old native-realm `thorium` user and role with the ECK superuser so no stale
  credentials or privileges are left behind, for example:

  ```bash
  PASS=$(kubectl -n elastic get secret elastic-es-elastic-user -o jsonpath='{.data.elastic}' | base64 -d)
  kubectl -n elastic port-forward svc/elastic-es-http 9200 &
  curl -sk -u "elastic:$PASS" -X DELETE https://localhost:9200/_security/user/thorium
  curl -sk -u "elastic:$PASS" -X DELETE https://localhost:9200/_security/role/thorium
  ```

- When `status.message` notes that an index doesn't map `group` as a keyword, the index was
  created from old dynamic mappings. Reindex it by running the search streamer once with
  `--reindex`, which deletes and recreates its indexes and streams every document from the
  database again (add `--reindex` to `operator.cluster.components.search_streamer.args`, wait
  for it to finish, then remove it).

## Proxies

`operator.operator.proxy` (`httpProxy`, `httpsProxy`, `noProxy`) sets the proxy environment of the
Thorium operator. When `httpProxy` or `httpsProxy` is set, the operator's `no_proxy`/`NO_PROXY` is
the cluster's own ranges and names followed by your `noProxy` entries:

- `localhost`, `127.0.0.1`, `.svc`, `.svc.<clusterDomain>`, and `<clusterDomain>`
  (`global.clusterDomain`, default `cluster.local`)
- every CIDR in `global.clusterCIDRs` (default empty)

Set `global.clusterCIDRs` to the cluster's service and pod CIDRs, which the chart can't detect
(the install notes print a warning when a proxy is set and it is empty; megathor sets it from
`thorium_cluster_cidrs`).
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

The Thorium image has no `USER`, so the operator runs as root unless
`operator.operator.podSecurityContext` says otherwise; that pod context defaults to the
runtime's `RuntimeDefault` seccomp profile, which only blocks system calls the operator never
makes. The operator only makes
network calls (Kubernetes API, the Thorium API, and the backends) and writes nothing to disk, so
its container defaults to `allowPrivilegeEscalation: false` and `capabilities.drop: [ALL]`
(`operator.operator.securityContext`; set a key to `null` to drop it). For the `restricted` Pod
Security level, also add `runAsNonRoot: true`, `runAsUser: 65534` and `runAsGroup: 65534` to
`podSecurityContext` and add `readOnlyRootFilesystem:
true` to `securityContext`; the files the operator reads (its service account token and the
optional Elastic CA) are mounted world-readable.

The `pre-delete` uninstall hook Job (`operator.uninstallHook`) deletes the `ThoriumCluster` and
waits for the operator to clean it up. It only runs `curl` and `jq` against the Kubernetes API
with a service account that may get and delete that one `ThoriumCluster`, so it is restricted by
default: user and group 65534, the `RuntimeDefault` seccomp profile, no privilege escalation,
every capability dropped, and a read-only root filesystem with an `emptyDir` at `/tmp` for its
auth header and responses (`uninstallHook.podSecurityContext`/`securityContext`). It uses
`operator.nodeSelector` and `operator.tolerations` like the operator, with requests of 10m CPU
and 32Mi memory and limits of 100m CPU and 64Mi memory (`uninstallHook.resources`).

## Offline

Pull or package both charts, mirror the images `charts/scripts/list-images.sh` prints (each at its
upstream path without the registry host), and install with `global.imageRegistry=<registry>`
on the thorium chart, plus `global.imageRegistry`, `scylla-operator.image.repository`, and
`eck-operator.image.repository` on infra-operators. `global.imageRegistry` also covers the
images the operators start (Elasticsearch, Kibana, Scylla, Thorium components).

The Thorium image is pulled with `imagePullPolicy: Always` by default, so a new image pushed under
the same tag is picked up whenever a pod starts. `operator.image.pullPolicy` sets the policy for
the operator and, through the ThoriumCluster, every Thorium component; set it to `IfNotPresent`
where the registry can't be reached but the image is already cached on the nodes.

## Versions

The charts are versioned with the rest of Thorium. `python3 .github/scripts/versions.py bump <version>`
writes one version into the Cargo workspace (including Cargo.lock and the Python crate), the web
UI's package files, every Thorium chart's `version`, `appVersion`, and subchart pins, the
minithor and megathor default chart versions, and the documentation examples. Without a version
it syncs everything to the current `Cargo.toml` workspace version. `versions.py check`, run by CI,
fails when any of them disagree.

## Credentials

```bash
kubectl -n thorium get secret thorium-admin -o jsonpath='{.data.password}' | base64 -d
kubectl -n thorium get secret thorium-credentials -o json | jq '.data | map_values(@base64d)'
```

Set any credential under `secrets.credentials` to supply your own. Existing values are never
regenerated while `thorium-credentials` can be read; to rotate one, set it explicitly (except
`thoriumSecretKey`, which must never change; see "Credentials that must not change or be lost").

## Known limitations

- The chart-managed Elasticsearch's TLS is not verified (`operator.backends.elastic.insecureCertificates`)
  because ECK serves a self-signed certificate whose CA the operator can't read; an external
  Elasticsearch can be verified with `operator.backends.elastic.caSecret`.
- After the Elastic CA Secret is rotated, the operator sees the new CA only once the kubelet
  refreshes its mounted file (typically up to about a minute), so the status may briefly show an
  Elastic TLS error before recovering on its own.
- The operator applies its own ThoriumCluster CRD schema at startup, so rolling the operator back
  to an older version also rolls the cluster-scoped CRD back.
- The Scylla operator's webhook certificate is generated by the chart (valid 10 years) instead
  of by cert-manager.
