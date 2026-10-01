
# Overview

This folder contains a set of Ansible playbooks for deploying Thorium on top of locally hosted baremetal servers or VMs. This assumes you have no access to external storage interfaces or other cloud native storage/dbs. The playbook prepares the cluster (Rook/Ceph storage, Traefik ingress) and then installs Thorium and its backing services with the Thorium Helm charts (`deploy/charts`): the `infra-operators` chart (Scylla, ECK, and Kubegres operators) and the `thorium` chart (Redis, Scylla, Elasticsearch, SeaweedFS, Quickwit, Jaeger, and Thorium itself), generating the chart values from the inventory group variables (`inventory/group_vars`). You can repurpose it for hosted environments by disabling components your environment already provides.

### Prerequisites

The host you run these Ansible roles from will need network connectivity to your Kubernetes cluster kube-api service along with a valid kube config in your home directory. You will also need an installation of python3 and pip.

Install prerequisites 

 - python3
 - pip

```bash
python3 -m pip install -r requirements.txt
ansible-galaxy collection install kubernetes.core amazon.aws community.general --upgrade
```

Alternatively you can package up all python dependencies using a virtual environment, this is most useful for using this playbook in environments with restricted internet access.

```bash
python3 -m pip install virtualenv
python3 -m virtualenv venv
source venv/bin/activate
python3 -m pip install -r requirements.txt
ansible-galaxy collection install kubernetes.core amazon.aws community.general --upgrade
```

Ansible galaxy modules are installed in `~/.ansible` and would need to be saved when conducting offline deployments.

### Kernel Parameters

Some systems may require setting certain kernel parameters for systems like Elastic to startup properly.

```
fs.aio-max-nr=2097152
fs.file-max=8097152
vm.max_map_count=262144
```

### File Staging (offline usage only)

```bash
ansible-playbook -i inventory/local.ini offline-stage.yml -v
```

For offline deploments, stage any nessesary files into the `./files` directory. These files will get moved up along with the full playbook. `offline-stage.yml` saves the Traefik and Rook charts and the packaged `thorium` and `infra-operators` charts (`files/thorium-<version>.tgz`, `files/infra-operators-<version>.tgz`), and writes `files/thorium-images.txt`, the images those charts deploy. To stage charts that are not published yet, pass `-e thorium_stage_charts_from=$PWD/../deploy/charts`.

Every container image must also be mirrored into your private registry under its upstream path without the registry host (for example `<offline_registry>/scylladb/scylla:6.2.3`). `scripts/mirror-images.bash <offline_registry> files/thorium-images.txt` pulls, retags, and pushes the charts' images from a machine with internet access; `scripts/offline-images.txt` lists the remaining (Traefik, Rook) images. Then set `offline_registry` in `inventory/group_vars/offline.yml` and add the host to the `[offline]` group in the inventory. Offline deployments install the staged charts with every image pointed at `offline_registry`.

### Usage

Update the `group_vars` located in `inventories/group_vars` to match the requirements of your environment; at minimum set `thorium_nodes` (and `traefik_external_ips` when Traefik is enabled), since the playbook refuses to run with the example `<...>` placeholders. This playbook generates all secret key/passwords unless specified in the inventory variables. An ansible vault is generated containing those secret values to ensure that susequent runs of the playbook do not regenerate/override those values. Create the vault password file (`artifacts/vault_pass` by default) before the first run. If the vault exists but can't be decrypted (for example the vault password was not passed), the playbook stops rather than generating new secrets, since the running databases keep the passwords they were created with.

Any machine with `kubectl`, `helm` (3.8+), and a valid kube config for the cluster can run the playbook; database setup is done in-cluster by the Thorium operator, so cluster DNS names don't need to resolve on the controller. (Rook deployments and external S3 still create the Quickwit bucket from the controller, which needs to reach `s3_endpoint`.) Online deployments install the published charts (`thorium_chart_repo`, `thorium_chart_version`); set `thorium_chart`/`infra_operators_chart` to a chart directory or packaged `.tgz` to test unpublished charts, and `thorium_values_files` to layer your own chart values over the generated ones. The initial Thorium admin user (`thorium_admin_username`, default `admin`) gets a generated password saved in the vault as `thorium_admin_password`. Deploy thorium and database dependencies on top of k8s:

```bash
ansible-playbook -i inventories/local.ini deploy.yml --ask-vault-pass -v
```

or

```bash
ansible-playbook -i inventories/local.ini deploy.yml --vault-pass-file artifacts/vault_pass --extra-vars "vault_password_file=artifacts/vault_pass" -v
```

### S3 Backend

Thorium and Quickwit need an S3-compatible object store. Enable at most one of the following backends (the playbook fails early if more than one is enabled); the `s3_endpoint`, `s3_region`, `s3_flavor`, `s3_thorium_region`, and `s3_use_path_style` settings are derived from whichever one is enabled (see `roles/s3/defaults/main.yml`).

| Toggle | Backend | Default in |
|--------|---------|------------|
| `rook_enabled` | Rook/Ceph object store (RGW), replicated across nodes; also provides the `ceph-block` storage class | `all.yml` |
| `seaweedfs_enabled` | Single-instance SeaweedFS (`weed mini`) on one PVC of `seaweedfs_storage_class` (defaults to `storage_class`) | `single_node.yml` |

SeaweedFS (deployed by the thorium chart) runs as a single replica, so it suits single-node or small deployments; use Rook for replicated storage across nodes. Its image is pinned in the chart because `weed mini` defaults can change between releases. SeaweedFS S3 identities are stored in the `seaweedfs-s3-config` secret and use the generated `s3_access_key`/`s3_secret_key`, and the Quickwit bucket is created by an in-cluster `weed shell` job (Thorium's own buckets are created by the Thorium operator). SeaweedFS's master and filer ports have no authentication, so the `seaweedfs-ingress` NetworkPolicy only lets other pods reach the S3 port; this needs a CNI that enforces NetworkPolicy (e.g. Calico or Cilium).

Both backends are reached through a single in-cluster service name, so Thorium uses path-style bucket addressing. With Rook the region is `thorium-s3`, the name Rook gives the Ceph zonegroup, which RGW requires as the bucket location constraint.

To use an external S3 service, disable both toggles and set `s3_endpoint` (required), `s3_region`, `s3_access_key`, `s3_secret_key`, and if needed `s3_use_path_style`/`s3_flavor` in `group_vars` directly.

The MinIO backend has been removed: the MinIO community edition no longer publishes container images. A deployment that used `minio_enabled` needs its objects migrated to SeaweedFS, Rook, or an external S3 service.

### Operators

The Scylla, ECK, and Kubegres operators are installed by the `infra-operators` chart into the `infra-operators` namespace (`infra_operators_namespace`). The Scylla operator's webhook certificate is generated by the chart, so cert-manager is not needed. Disable any operator the cluster already runs with `scylla_operator_enabled`, `elastic_operator_enabled`, or `kubegres_operator_enabled`, or skip the chart entirely with `infra_operators_enabled: false`.

### Scylla Developer Mode

`scylla_developer_mode` (default `false`, `true` in `single_node.yml`) relaxes Scylla's startup checks; production mode refuses to start without XFS storage and tuned hosts.

### Multiple Deployments in One Cluster

Setting `thorium_instance` prefixes every per-deployment namespace with `<thorium_instance>-` (`test-thorium`, `test-redis`, `test-scylla`, `test-elastic`, `test-quickwit`, `test-seaweedfs`, `test-jaeger`), so several Thorium deployments can share one Kubernetes cluster for testing. Each deployment runs its own Thorium operator, which only manages its own namespace.

The `infra-operators` chart and Traefik are cluster-wide, so exactly one deployment in the cluster should deploy them; set `infra_operators_enabled: false` and `traefik_enabled: false` in every other deployment's inventory. Deployments sharing a Traefik must each set a distinct `thorium_ingress_hostname`, and only the one running Traefik provides Traefik's default TLS certificate. Each deployment's scaler creates job namespaces named after Thorium groups, so run the scaler in only one deployment (`thorium_scaler_enabled: false` elsewhere).

### Cleanup

These commands remove what the playbook deploys, which helps when testing the playbooks without redeploying your k8s environment. For a named instance, prefix each Thorium namespace with `<thorium_instance>-`.

Remove Thorium and its backing services (delete the ThoriumCluster first so the Thorium operator can clean up):

```bash
kubectl delete thoriumcluster thorium -n thorium
helm uninstall thorium -n thorium
```

The namespaces (`thorium`, `redis`, `scylla`, `elastic`, `seaweedfs`, `quickwit`, `jaeger`), their PVCs, and the `thorium-credentials` secret are kept on uninstall; delete the namespaces to remove the data:

```bash
kubectl delete namespace thorium redis scylla elastic seaweedfs quickwit jaeger
```

Remove the operators and Traefik once no Thorium deployment remains (Helm leaves the Scylla and Kubegres CRDs installed):

```bash
helm uninstall infra-operators -n infra-operators
helm uninstall traefik -n traefik
```
