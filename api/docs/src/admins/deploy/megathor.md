# Production Clusters (megathor)

megathor is a set of Ansible playbooks (the `megathor/` directory of the Thorium repository)
that deploys Thorium onto an existing Kubernetes cluster running on bare-metal servers or VMs.
It prepares what such a cluster usually lacks (Rook/Ceph storage and a Traefik ingress), then
installs the Thorium Helm charts with values it generates from the inventory. Use it for
multi-node production clusters without cloud storage or managed databases. For a cluster that
already provides those, [install the charts directly](./deploy-helm.md); for a laptop or test
cluster, use [minithor](./minithor.md).

```bash
cd megathor
openssl rand -base64 32 > artifacts/vault_pass && chmod 600 artifacts/vault_pass
ansible-playbook -i inventory/local.ini deploy.yml --vault-password-file artifacts/vault_pass -v
```

## What the playbook does

`deploy.yml` runs these roles in order against `localhost` (the controller):

| Role | Runs when | What it does |
|------|-----------|--------------|
| `common` | always | Checks the inventory (at most one S3 backend, an external S3 endpoint when none is deployed, the namespace prefix, external credentials, no `<...>` placeholders in `thorium_nodes` or `traefik_external_ips`, no unspecified, loopback, or link-local `traefik_external_ips`, a supported `traefik_service_type`) and creates `artifacts/` |
| `rook` | `rook_enabled` | Installs the Rook operator and a Ceph cluster from the Rook Helm charts (`rook_helm_chart_version`), with an RGW object store and the `ceph-block` storage class |
| `secrets` | always | Loads or creates the credentials vault (see [Credentials and the vault](#credentials-and-the-vault)) |
| `s3` | `seaweedfs_enabled` is false and `quickwit_enabled` | Creates the Quickwit bucket in Rook (from the `rook-ceph-tools` pod) or in an external S3 service (from the controller) |
| `traefik` | `traefik_enabled` | Installs Traefik from its Helm chart (`traefik_helm_chart_version`) and waits for it (see `traefik_wait`) |
| `infra_operators` | `infra_operators_enabled` | Installs the `infra-operators` chart (Scylla, ECK, and Kubegres operators) |
| `thorium` | `thorium_enabled` | Renders the `thorium` chart values from `roles/thorium/templates/values.yaml.j2`, installs the chart as release `thorium` in the thorium namespace, and waits for the operator (see [Waiting for Thorium](#waiting-for-thorium)) |

Database setup (Scylla role, Elastic user, buckets) happens in the cluster, done by the Thorium
operator, so cluster DNS names don't need to resolve on the controller. The generated values file
holds every credential, so it only exists on disk (`artifacts/thorium-values.yaml`, mode 0600)
while Helm runs.

## Prerequisites

- An existing Kubernetes cluster, and a controller with network access to its API server, a valid
  kube config, `kubectl`, and Helm 3.8 or newer.
- python3 and pip on the controller, then:

  ```bash
  python3 -m pip install -r requirements.txt
  ansible-galaxy collection install kubernetes.core amazon.aws community.general --upgrade
  ```

  For restricted networks, install the same packages into a virtual environment you can copy
  (`python3 -m virtualenv venv && source venv/bin/activate` first). Galaxy collections land in
  `~/.ansible`, which must be copied too.
- Kernel settings on every node that runs Scylla or Elasticsearch:

  ```
  fs.aio-max-nr=2097152
  fs.file-max=8097152
  vm.max_map_count=262144
  ```

- Scylla in production mode (`scylla_developer_mode: false`, the default) refuses to start on
  hosts that don't meet its requirements: provision XFS on NVMe storage with adequate CPU and
  memory. See [Planning a Deployment](./planning.md).

## Inventory

`inventory/local.ini` has three groups:

| Group | Meaning |
|-------|---------|
| `[single_node]` | Ships with `localhost`, so `inventory/group_vars/single_node.yml` applies: SeaweedFS instead of Rook, one Scylla member with replication 1, one Elasticsearch node, small resource sizes, a `NodePort` Traefik Service, and placeholder `traefik_external_ips` (`< IP 1>` ...) that must be replaced. **Remove `localhost` from this group for a multi-node production cluster**, so the defaults in `group_vars/all.yml` and the role defaults apply |
| `[offline]` | Add `localhost` here for an air-gapped install; `group_vars/offline.yml` then applies (see [Offline installs](#offline-installs)) |
| `[local]` | The connection settings for `localhost` |

Edit `inventory/group_vars/*.yml` (or pass `-e`/`--extra-vars`) for your environment. The playbook
refuses to run while `thorium_nodes` or `traefik_external_ips` contain `<...>` placeholders, or while
`traefik_external_ips` holds an address Kubernetes rejects in a Service's `externalIPs`
(unspecified such as `0.0.0.0` or `::`, loopback, or link-local).

## Credentials and the vault

megathor generates Thorium's secret key and the backing services' passwords once and keeps them in
an Ansible vault (`vault_path`, default `artifacts/secrets.yml`), encrypted with
`vault_password_file` (default `artifacts/vault_pass`). The password file is mandatory: create it
before the first run and pass it to `ansible-playbook` with `--vault-password-file`. With a
different file, pass it both ways:
`--vault-password-file <path> --extra-vars vault_password_file=<path>`.

Each credential comes from the first of:

1. the inventory or `--extra-vars`,
2. the vault,
3. the `thorium-credentials` Secret of an existing deployment (which the chart keeps on
   uninstall),
4. a newly generated value,

and whatever is used is written back to the vault. A lost vault is rebuilt from
`thorium-credentials`. The playbook refuses to generate a credential when Thorium is already
deployed in `thorium_namespace` but neither the vault nor `thorium-credentials` holds it (a new
secret key locks out every user, and new passwords break every backend still using the old ones),
and it stops when the vault exists but can't be decrypted.

| Vault variable | `thorium-credentials` key |
|----------------|---------------------------|
| `thorium_secret` | `thoriumSecretKey` |
| `redis_password` | `redisPassword` |
| `scylla_password` | `scyllaPassword` |
| `elastic_password` | `elasticPassword` |
| `s3_access_key` | `s3AccessKey` |
| `s3_secret_key` | `s3SecretKey` |
| `quickwit_postgres_super_password` | `postgresPassword` |
| `thorium_admin_password` | `adminPassword` |

Credentials megathor never generates or vaults, which you must set yourself (keep them in an
`ansible-vault` encrypted vars file, for example
`ansible-vault encrypt_string --name redis_password '<password>'`):

- `redis_password` for an external Redis (`redis_enabled: false`),
- `s3_access_key` and `s3_secret_key` for an external S3 service (Rook's keys are read from Ceph
  on every run),
- `thorium_external_scylla_password` / `thorium_external_elastic_password` for an external Scylla
  or Elasticsearch without an admin secret.

Task output that holds credentials is hidden (`no_log`), so `-v` doesn't print them. Back up the
vault and its password file along with [the rest of your backups](./operate.md#backups).

## Variables

### Charts and deployment

| Variable | Default | Meaning |
|----------|---------|---------|
| `thorium_chart_repo` | `oci://ghcr.io/cisagov/thorium/charts` | Where the published charts come from |
| `thorium_chart_version` | the pinned chart version | Version of both charts |
| `thorium_chart` / `infra_operators_chart` | `""` | A chart directory, packaged `.tgz`, or `oci://` reference to use instead (for example `../deploy/charts/thorium` to test unpublished charts) |
| `thorium_values_files` | `[]` | Extra values files for the thorium chart, applied after the generated values (use absolute paths) |
| `infra_operators_values_files` | `[]` | Extra values files for the infra-operators chart, applied after the generated values (use absolute paths) |
| `namespace_prefix` | `""` | Prefix for every Thorium namespace (see [Namespaces and the namespace prefix](./concepts.md#namespaces-and-the-namespace-prefix)) |
| `thorium_namespace` | `<namespace_prefix>-thorium` or `thorium` | Derived from `namespace_prefix`; the playbook refuses a value that doesn't match |
| `thorium_deployment_name` | `thorium` | The `ThoriumCluster` name (`operator.cluster.name`) |
| `thorium_ready_timeout` | `1800` | Seconds to wait for the operator |
| `storage_class` | `local-path` (`all.yml`) | Storage class for every PVC (`ceph-block` with Rook) |
| `infra_operators_enabled` | `true` | Install the infra-operators chart into `infra_operators_namespace` (`infra-operators`) |
| `scylla_operator_enabled`, `elastic_operator_enabled`, `kubegres_operator_enabled` | `true` | Turn off any operator the cluster already runs |
| `scylla_operator_replicas` | `1` | Replicas of the Scylla operator and its webhook server |
| `traefik_enabled` | `true` | Install Traefik (`traefik_namespace` `traefik`, `traefik_replicas`, `traefik_helm_chart_version`) |
| `traefik_service_type` | `LoadBalancer` (`NodePort` in `single_node.yml`) | The type of Traefik's Service (`LoadBalancer`, `NodePort`, or `ClusterIP`). A `LoadBalancer` Service only gets an address from a load balancer controller (MetalLB, a cloud provider) or from `traefik_external_ips`; on bare metal without either, use `NodePort` |
| `traefik_external_ips` | `[]` | The `externalIPs` of Traefik's Service: the node addresses Traefik should be reachable on (ports 80 and 443, whatever the Service type). Empty leaves `externalIPs` out of the Service. Kubernetes rejects `0.0.0.0` here, so an inventory listing it must switch to node addresses or `[]` |
| `traefik_wait` | `true` unless the Service is a `LoadBalancer` without `traefik_external_ips` | Whether the Helm install waits for the whole release. Helm counts a `LoadBalancer` Service as ready only once it has `externalIPs` or a load balancer address, so without them the role waits only for Traefik's Deployment (up to `service_wait_timeout`); set it to `true` when a load balancer controller assigns the address |
| `rook_enabled` | `true` (`all.yml`) | Install Rook/Ceph (`rook_namespace` `rook-ceph`, `rook_helm_chart_version`; replica counts, failure domain, and RGW resources in `roles/rook/defaults/main.yml`) |
| `ceph_ignore_health_errors`, `ceph_health_check_retries` | `true`, `1` (`all.yml`) | Whether a Ceph cluster that isn't `HEALTH_OK` after the retries (10 seconds apart) only warns instead of failing the run |
| `service_wait_timeout` | `300` (`all.yml`) | Seconds to wait for the Rook pods, the infra-operators chart, and Traefik's Deployment when `traefik_wait` is off |
| `offline`, `offline_registry` | `false`, unset | Install offline from the staged charts with every image pulled from `offline_registry` (see [Offline installs](#offline-installs)) |
| `k8s_registry`, `rook_container_registry`, `ceph_container_registry`, `traefik_container_registry` | the upstream registries | Where the Rook, Ceph CSI, and Traefik images come from (`offline_registry` in `offline.yml`) |
| `thorium_stage_charts_from` | unset | `offline-stage.yml` only: a `deploy/charts` directory to package the charts (and take `list-images.sh`) from |

### Thorium

| Variable | Default | Meaning |
|----------|---------|---------|
| `thorium_container_registry` / `thorium_image` | `ghcr.io` / `cisagov/thorium/infrastructure/thorium` | The Thorium image (offline installs pull it from `offline_registry`) |
| `thorium_version` | `""` | Image tag; empty uses the chart's appVersion |
| `thorium_image_pull_policy` | `Always` | `IfNotPresent` runs from images cached on the nodes |
| `thorium_image_pull_secret` | `{}` | A `.dockerconfigjson` document (as YAML) rendered into the `thorium-image-pull` Secret |
| `thorium_ui_banner` | placeholder text | The login banner |
| `thorium_admin_username` | `admin` | The initial admin user; its password is generated into the vault as `thorium_admin_password` |
| `thorium_nodes` | `[]` | Nodes Thorium may schedule jobs on; empty means every node |
| `thorium_scaler_enabled` | `true` | Run the k8s scaler |
| `thorium_ingress_enabled` | `true` | Route the API through Traefik (needs Traefik's CRDs) |
| `thorium_ingress_hostname` | unset | The hostname to route; unset or `*` routes every host |
| `thorium_tls_cert_secret_name` | unset | An existing `kubernetes.io/tls` Secret in the thorium namespace |
| `thorium_operator_{cpu,memory}_{requests,limits}` | `256m`/`256Mi` requests, `512m`/`512Mi` limits | The operator's resources |
| `thorium_cluster_cidrs` | `[]` | The cluster's service and pod CIDRs (see [Proxies](#proxies)) |
| `thorium_upgrade_target_revision` | `""` | The revision to upgrade to (see [Upgrades](#upgrades)) |
| `thorium_upgrade_approvals` | `[]` | Approvals for upgrade steps that change data: `[{step: <id>, backup: <where>}]` |

### Backing services

| Variable | Default | Meaning |
|----------|---------|---------|
| `redis_enabled` | `true` | Deploy Redis (`redis_volume_size`, `redis_{cpu,memory}_{requests,limits}`) |
| `scylla_enabled` | `true` | Deploy Scylla (`scylla_members` 3, `scylla_replication_factor` 3, `scylla_developer_mode` false, `scylla_storage_size`, resources) |
| `elastic_enabled` | `true` | Deploy Elasticsearch (`elastic_search_node_count`, `elastic_search_volume_size`, resources) |
| `kibana_enabled` | `true` | Deploy Kibana |
| `seaweedfs_enabled` | `false` (`true` in `single_node.yml`) | Deploy SeaweedFS as the S3 store (`seaweedfs_volume_size`, resources) |
| `quickwit_enabled` | `true` | Deploy Quickwit, its Postgres metastore, and Jaeger |
| `quickwit_bucket` | `quickwit` | The bucket Quickwit stores traces in |
| `quickwit_metastore_uri` | `""` | An external Quickwit metastore database; setting it skips the chart's Postgres |
| `quickwit_postgres_replicas` | `1` | Instances of the chart's Quickwit metastore Postgres |
| `quickwit_postgres_storage_size` | `32G` (`16G` in `single_node.yml`) | The volume size of each Quickwit metastore Postgres instance. The misspelled `quickwit_postgress_storage_size` is deprecated but still read when `quickwit_postgres_storage_size` isn't set |
| `jaeger_enabled` | `true` | Deploy Jaeger (only together with `quickwit_enabled`) |
| `thorium_tracing_endpoint` | `""` | Another OTLP gRPC collector for Thorium's traces |

The per-service sizes are listed in `roles/thorium/defaults/main.yml` and overridden for single
nodes in `group_vars/single_node.yml`.

### How variables map to chart values

| megathor variable | thorium chart value |
|-------------------|---------------------|
| `offline` + `offline_registry` | `global.imageRegistry` |
| `thorium_cluster_cidrs` | `global.clusterCIDRs` |
| `scylla_enabled`, `elastic_enabled`, `redis_enabled` | `global.managed.scylla`, `.elastic`, `.redis` |
| `seaweedfs_enabled` | `global.managed.s3` |
| `quickwit_enabled` and an empty `quickwit_metastore_uri` | `global.managed.postgres` |
| `quickwit_enabled`, `s3_endpoint`, `quickwit_metastore_uri` | `global.quickwit.enabled`, `.s3Endpoint`, `.metastoreUri` |
| vault credentials, `thorium_admin_username` | `secrets.credentials.*` |
| `storage_class` and the size variables | `infra.storageClass`, `infra.<service>.*` |
| `jaeger_enabled` and `quickwit_enabled` | `infra.jaeger.enabled` |
| `quickwit_bucket`, `s3_region`, `s3_flavor`, replica counts | `quickwit.config`, `quickwit.metastore`, `quickwit.searcher` |
| `thorium_container_registry`/`thorium_image`, `thorium_version`, `thorium_image_pull_policy` | `operator.image.*` |
| `thorium_image_pull_secret` | `operator.createImagePullSecret`, `operator.dockerConfigJson` |
| `thorium_ui_banner` | `operator.banner` |
| `thorium_operator_*` | `operator.controller.resources` |
| `thorium_deployment_name` | `operator.cluster.name` |
| `thorium_scaler_enabled: false` | `operator.cluster.components.scaler: null` |
| `thorium_nodes` | `operator.cluster.scaler.k8s.nodes` |
| `thorium_upgrade_target_revision`, `thorium_upgrade_approvals` | `operator.cluster.upgrade.targetRevision`, `.approvals` |
| `thorium_scylla_admin_secret`, `thorium_elastic_admin_secret` | `operator.cluster.bootstrap.scylla`, `.elastic` |
| `thorium_external_*`, `scylla_replication_factor`, `s3_endpoint`, `s3_thorium_region`, `s3_use_path_style`, `thorium_tracing_endpoint` | `operator.backends.*` |
| `thorium_ingress_enabled`, `thorium_ingress_hostname`, `thorium_tls_cert_secret_name` | `operator.ingress.type` (`traefik` or `none`), `.host`, `.tls.secretName` |
| `traefik_enabled` | `operator.ingress.tls.traefikDefaultStore` |

Anything else goes in a file listed in `thorium_values_files`, for example an external
Elasticsearch's CA (`operator.backends.elastic.caSecret`), the operator's proxy
(`operator.controller.proxy`), CORS (`operator.cluster.cors`), or pod security settings. See
[Configure the Helm Charts](./helm-configuration.md) and the
[Chart Values Reference](./chart-values.md).

## S3 backends

Thorium and Quickwit need an S3-compatible store. Enable at most one backend (the playbook fails
when both are on); `s3_endpoint`, `s3_region`, `s3_flavor`, `s3_thorium_region`, and
`s3_use_path_style` are derived from it (`roles/s3/defaults/main.yml`):

| Backend | Toggle | Notes |
|---------|--------|-------|
| Rook/Ceph RGW | `rook_enabled` (default in `all.yml`) | Replicated across nodes; also provides the `ceph-block` storage class. Region `thorium-s3` (the Ceph zonegroup name, which RGW requires as the bucket location constraint); Quickwit flavor `minio` |
| SeaweedFS | `seaweedfs_enabled` (default in `single_node.yml`) | One `weed mini` process on one PVC, deployed by the chart; suits single-node or small clusters. Its `seaweedfs-ingress` NetworkPolicy only admits the S3 port, which needs a CNI that enforces NetworkPolicy (Calico, Cilium) |
| External S3 | both off | Set `s3_endpoint`, `s3_access_key`, and `s3_secret_key` (all required), and if needed `s3_region`, `s3_use_path_style`, `s3_flavor`, and `s3_validate_certs` (default `true`, for the controller's Quickwit bucket request) |

The in-cluster backends are reached through one service name, so Thorium uses path-style bucket
addressing. Thorium's own buckets are created by the operator; the Quickwit bucket is created by
the chart in SeaweedFS, and by the `s3` role in Rook or an external service (the run fails if it
can't; an external service must be reachable from the controller). Without Quickwit no bucket is
created. megathor supports these three backends only; a deployment that stored its objects in MinIO
must migrate them to one of them.

## External services

Each backing service has a toggle megathor passes to the chart as `global.managed.<service>`.
Turn one off and set its external settings (see
[External services](./helm-configuration.md#external-services) for what each service must provide):

| Toggle | External settings |
|--------|-------------------|
| `scylla_enabled: false` | `thorium_external_scylla_nodes`, and either `thorium_scylla_admin_secret` (the operator creates Thorium's role) or the role you created in `thorium_external_scylla_username` / `thorium_external_scylla_password` |
| `elastic_enabled: false` | `thorium_external_elastic_node`, and either `thorium_elastic_admin_secret` (the operator creates Thorium's user) or the user you created in `thorium_external_elastic_username` / `thorium_external_elastic_password` |
| `redis_enabled: false` | `thorium_external_redis_host` (and `thorium_external_redis_port`, default 6379) and `redis_password` |
| `seaweedfs_enabled` and `rook_enabled` both false | `s3_endpoint`, `s3_access_key`, `s3_secret_key` (see [S3 backends](#s3-backends)) |
| `quickwit_metastore_uri` set | The URI of a Quickwit metastore database you created |

The admin secrets name Secrets in the thorium namespace with `username` and `password` keys. Without
one, the playbook and the chart refuse to run until the matching external password is set, since a
generated password would never match the role or user you created.

Without Quickwit (`quickwit_enabled: false`) the chart deploys neither Quickwit, its Postgres, nor
Jaeger, and Thorium sends no external traces unless `thorium_tracing_endpoint` names another OTLP
gRPC collector; the playbook warns when it is unset.

## Ingress and TLS

With `thorium_ingress_enabled` (the default) the chart renders a Traefik `IngressRoute` on the
`websecure` entrypoint that routes `thorium_ingress_hostname` (or every host) to the
`thorium-api` service. Create the certificate Secret in the thorium namespace before deploying
and name it in `thorium_tls_cert_secret_name`:

```bash
kubectl -n thorium create secret tls api-certs --cert=thorium.crt --key=thorium.key
```

When megathor deploys Traefik (`traefik_enabled`), that certificate also becomes Traefik's default
certificate (the cluster-wide `default` TLSStore, `operator.ingress.tls.traefikDefaultStore`).
Traefik allows only one default certificate, so with a Traefik megathor didn't install the chart
leaves the default store alone.

## Proxies

If the Thorium operator reaches the internet through a proxy (set with `operator.controller.proxy`
in a `thorium_values_files` file), set `thorium_cluster_cidrs` to the cluster's service and pod
CIDRs (for example `["10.96.0.0/12", "10.244.0.0/16"]`). They become the chart's
`global.clusterCIDRs`, which is added to the operator's `noProxy` so in-cluster traffic addressed
by IP bypasses the proxy. megathor can't detect these CIDRs. See
[Proxies](./helm-configuration.md#proxies).

## Offline installs

1. On a machine with internet access, stage the charts and the image list with the same inventory
   and `-e` overrides you deploy with:

   ```bash
   ansible-playbook -i inventory/local.ini offline-stage.yml -v
   ```

   It saves the Traefik and Rook charts (when enabled), the packaged `thorium` and
   `infra-operators` charts (`files/thorium-<thorium_chart_version>.tgz`,
   `files/infra-operators-<thorium_chart_version>.tgz`), and writes `files/offline-images.txt`,
   every image the deployment pulls, rendered from the same values `deploy.yml` generates
   (including `thorium_image`/`thorium_version`, `thorium_values_files`, and
   `infra_operators_values_files`) and always naming each image's upstream source. To stage
   unpublished charts, add `-e thorium_stage_charts_from=$PWD/../deploy/charts`; they are packaged
   as `thorium_chart_version`. The image list comes from `deploy/charts/scripts/list-images.sh`,
   which isn't part of the packaged charts, so run `offline-stage.yml` from a full Thorium
   repository checkout (`megathor/` next to `deploy/`); with a copy of `megathor/` on its own, set
   `thorium_stage_charts_from` to the `deploy/charts` directory of a checkout instead.
2. Mirror every image into your registry under its upstream path without the registry host (for
   example `<offline_registry>/scylladb/scylla:6.2.3`):

   ```bash
   scripts/mirror-images.bash <offline_registry> [image-list]
   ```

   The list defaults to `files/offline-images.txt`; the script uses docker, or podman when docker
   is missing.
3. Copy the playbook directory (with `files/`, and your venv and `~/.ansible` if needed) to the
   offline controller, set `offline_registry` in `inventory/group_vars/offline.yml`, add
   `localhost` to `[offline]`, and run `deploy.yml`. The staged charts are installed with every
   image pointed at `offline_registry`.

## Waiting for Thorium

After installing the chart, the playbook waits up to `thorium_ready_timeout` seconds for the
operator to report on the current spec of the `ThoriumCluster` in `thorium_namespace`:

- `Ready`: the run succeeds and prints the admin user and where its password is (the Secret the
  admin bootstrap reads, usually `thorium-admin`, and the vault's `thorium_admin_password` when the
  two match; a values file such as a conversion's can set another login), or, when the cluster's
  admin bootstrap is off (such as a converted deployment without an admin login), that no admin
  user was created.
- `UpgradeRequired`: the run succeeds and prints the operator's message and the planned steps;
  set a target revision and run again (see [Upgrades](#upgrades)).
- `Error`, or an upgrade step that is `Blocked`: the run fails at once with the operator's message
  and the blocked steps.

See [Troubleshooting Deployments](./troubleshooting.md) for the messages.

## Upgrades

To upgrade, set `thorium_chart_version` (and `thorium_version` if you pin the image) and run the
playbook again. When the new operator knows a newer revision than the deployment's, the
`ThoriumCluster` waits in `UpgradeRequired` and the playbook prints the steps. Set
`thorium_upgrade_target_revision` to that revision and run again. Steps that change data and have
work to do wait for an approval naming the backup you took first:

```yaml
thorium_upgrade_target_revision: "2026-10-v02"
thorium_upgrade_approvals:
  - step: elastic-reindex-keyword-mappings
    backup: "thoradm backup 2026-10-08 on /mnt/backups"
```

See [Upgrading Thorium](./upgrades.md) for what each revision does.

## Converting a pre-Helm megathor deployment

A deployment made by a megathor from before the Helm charts (a `ThoriumCluster` named `dev` with
inline credentials) must be converted once before this playbook runs against it; the operator and
the chart refuse to manage it otherwise. Follow
[Converting a Pre-Helm Deployment](./convert-to-helm.md) (run `convert-to-helm.sh` with
`--namespace-prefix <namespace_prefix>` when a prefix is set), then run the playbook with:

```yaml
thorium_deployment_name: dev
infra_operators_enabled: false
thorium_values_files:
  - /path/to/thorium-converted-values.yaml   # last, so its values win
thorium_upgrade_target_revision: "2026-10-v02"
```

Add `thorium_upgrade_approvals` if the reindex step blocks (see
[Reindexing Elasticsearch](./upgrades.md#reindexing-elasticsearch)).

The converted values are applied after megathor's own, so their admin bootstrap setting wins. Pass
an existing admin's login to the conversion (`--admin-user` with the `THORIUM_ADMIN_PASSWORD`
environment variable or `--admin-password-stdin`); without one the bootstrap stays off, megathor's
generated `thorium_admin_password` creates no user, and the playbook's final message says that no
admin user was created. Remove every other pre-Helm instance from the cluster before the run (see
[Retiring other pre-Helm instances](./convert-to-helm.md#retiring-other-pre-helm-instances)).

Keep the inventory you deployed with, and in particular:

- **The vault.** Run with the old `artifacts/secrets.yml` and its password. The old megathor could
  prompt for the password (`--ask-vault-password`); the current one needs it in a file, so write it
  to `artifacts/vault_pass` (mode 0600) first. The credentials are reused from the vault, a new
  `thorium_admin_password` is added, and entries the current megathor doesn't use (such as
  `quickwit_postgres_rep_password`) are kept.
- **The S3 backend toggles.** Leave `rook_enabled` and `seaweedfs_enabled` as they were, so Rook
  stays managed and its S3 keys (or the SeaweedFS keys in the vault) are still found.

The old Redis, Scylla, Elasticsearch, SeaweedFS, Quickwit, Postgres, and Jaeger keep running where
the old megathor put them, as external services the converted values point Thorium at; megathor
no longer updates or removes them. To roll back before the playbook has installed the chart, run
the old megathor again and follow [Rolling back](./convert-to-helm.md#rolling-back).

## Removing a deployment

These commands remove what the playbook deploys (prefix each Thorium namespace with
`<namespace_prefix>-` when a prefix is set). The chart's `pre-delete` hook deletes the
`ThoriumCluster` first and waits for the operator to clean up:

```bash
helm uninstall thorium -n thorium
```

If the operator isn't running the hook times out; uninstall with `--no-hooks`, and if the
`ThoriumCluster` stays `Terminating`, remove its finalizer:

```bash
helm uninstall thorium -n thorium --no-hooks
kubectl -n thorium patch thoriumcluster thorium --type merge -p '{"metadata":{"finalizers":null}}'
```

The namespaces, their PVCs, the `thorium-credentials` Secret, and the group namespaces the k8s
scaler created are kept. Delete them to remove the data:

```bash
kubectl delete namespace thorium redis scylla elastic seaweedfs quickwit jaeger
kubectl delete namespace -l app.kubernetes.io/managed-by=thorium-scaler
```

Remove the operators and Traefik after Thorium (Rook, if megathor installed it, last: follow
Rook's own teardown guide, since its object store and block volumes hold data). Helm leaves the Scylla and Kubegres CRDs (from the
charts' `crds/` directories), the ECK CRDs (`helm.sh/resource-policy: keep`), and the
`ThoriumCluster` CRD the operator applies; deleting a CRD deletes every resource of that kind in
the cluster:

```bash
helm uninstall infra-operators -n infra-operators
helm uninstall traefik -n traefik
kubectl delete crd -l app.kubernetes.io/instance=infra-operators
kubectl get crd -o name | grep -E '\.scylla\.scylladb\.com$|/kubegres\.kubegres\.reactive-tech\.io$|/thoriumclusters\.sandia\.gov$' | xargs -r kubectl delete
```

See [Operating a Deployment](./operate.md#uninstalling) for what uninstalling keeps.
