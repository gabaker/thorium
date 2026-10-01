
# Overview

This folder contains a set of Ansible playbooks for deploying Thorium on top of locally hosted baremetal servers or VMs. This assumes you have no access to external storage interfaces or other cloud native storage/dbs. That said you can repurpose the `deploy.yml` playbook and inventory group variables (`inventory/group_vars`) for hosted environments by disabling roles that are made redundant by other services within your environment. You may have to conduct some steps manually, such as creating Elastic indexes when running this in hosted environments.

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

For offline deploments, stage any nessesary files into the `./files` directory. These files will get moved up along with the full playbook.

Every container image must also be mirrored into your private registry under its upstream path (for example `<offline_registry>/scylladb/scylla:6.2.3`). `scripts/offline-images.txt` lists the images a default deployment pulls, and `scripts/mirror-images.bash <offline_registry>` pulls, retags, and pushes them from a machine with internet access. Then set `offline_registry` in `inventory/group_vars/offline.yml` and add the host to the `[offline]` group in the inventory.

### Usage

Update the `group_vars` located in `inventories/group_vars` to match the requirements of your environment; at minimum set `thorium_nodes` (and `traefik_external_ips` when Traefik is enabled), since the playbook refuses to run with the example `<...>` placeholders. This playbook generates all secret key/passwords unless specified in the inventory variables. An ansible vault is generated containing those secret values to ensure that susequent runs of the playbook do not regenerate/override those values. Create the vault password file (`artifacts/vault_pass` by default) before the first run. If the vault exists but can't be decrypted (for example the vault password was not passed), the playbook stops rather than generating new secrets, since the running databases keep the passwords they were created with.

Any machine with `kubectl`, `helm`, and a valid kube config for the cluster can run the playbook: the Elastic, Postgres, and Scylla setup steps run inside their pods, so cluster DNS names don't need to resolve on the controller. (Rook deployments and external S3 still create the Quickwit bucket from the controller, which needs to reach `s3_endpoint`.) Deploy thorium and database dependencies on top of k8s:

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

SeaweedFS runs as a single replica, so it suits single-node or small deployments; use Rook for replicated storage across nodes. Its image is pinned (`seaweedfs_version`) because `weed mini` defaults can change between releases. SeaweedFS S3 identities are stored in the `seaweedfs-s3-config` secret and use the generated `s3_access_key`/`s3_secret_key`, and the Quickwit bucket is created by an in-cluster `weed shell` job (Thorium's own buckets are created by the Thorium operator). SeaweedFS's master and filer ports have no authentication, so the `seaweedfs-ingress` NetworkPolicy only lets other pods reach the S3 port; this needs a CNI that enforces NetworkPolicy (e.g. Calico or Cilium).

Both backends are reached through a single in-cluster service name, so Thorium uses path-style bucket addressing. With Rook the region is `thorium-s3`, the name Rook gives the Ceph zonegroup, which RGW requires as the bucket location constraint.

To use an external S3 service, disable both toggles and set `s3_endpoint` (required), `s3_region`, `s3_access_key`, `s3_secret_key`, and if needed `s3_use_path_style`/`s3_flavor` in `group_vars` directly.

The MinIO backend has been removed: the MinIO community edition no longer publishes container images. A deployment that used `minio_enabled` needs its objects migrated to SeaweedFS, Rook, or an external S3 service.

### Scylla Operator Version

The Scylla operator chart is pinned (`scylla_helm_chart_version`) because newer operators use `curl` in probes/hooks, which the pinned `scylla-manager-agent` image lacks. Operator values are read from `roles/scylla/templates/scylla/operator/<version>/operator.yaml.j2`, so a new version needs a matching values template. Helm does not upgrade CRDs of an already installed chart, so when upgrading an existing deployment apply the new chart's CRDs first:

```bash
helm pull scylla/scylla-operator --version v1.21.1 --untar --untardir /tmp/scylla-operator
kubectl apply --server-side --force-conflicts -f /tmp/scylla-operator/scylla-operator/crds/
```

Scylla Operator documents upgrades as one minor version at a time, so an existing deployment on an older version may need to step through the intermediate chart versions.

### Scylla Developer Mode

`scylla_developer_mode` (default `false`) relaxes Scylla's startup checks. Megathor targets production deployments, so leave it disabled and provision Scylla hosts accordingly (XFS on NVMe storage, adequate CPU and memory, tuned hosts); production mode refuses to start without them.

### Namespace Prefix

Setting `namespace_prefix` prefixes every per-deployment namespace with `<namespace_prefix>-` (`prod-thorium`, `prod-redis`, `prod-scylla`, `prod-elastic`, `prod-quickwit`, `prod-seaweedfs`, `prod-jaeger`). The cluster-role binding for the deployment's `thorium` service account is suffixed the same way.

### Cleanup

This section documents commands for cleaning up k8s resources that are deployed by these ansible roles. Cleanup can be helpful for testing the playbooks without redploying your k8s environment. Delete resources in the following order while confirming all the resources from one section have been deleted before moving to the next section.

Cleanup Thorium and DBs/elastic

```
# when namespace_prefix is set, prefix each namespace below with "<namespace_prefix>-"
kubectl delete ThoriumCluster dev -n thorium
helm uninstall -n traefik traefik
helm uninstall -n quickwit quickwit
kubectl delete statefulset -n redis redis
kubectl delete kibana -n elastic-system elastic
kubectl delete elasticsearch -n elastic-system elastic
kubectl delete Kubegres -n quickwit postgres
kubectl delete scyllacluster -n scylla scylla
kubectl delete statefulset.apps/jaeger -n jaeger
kubectl delete statefulset.apps/seaweedfs -n seaweedfs # when seaweedfs_enabled
```

Remove operators and cert-manager

```bash
helm uninstall -n scylla-operator scylla
kubectl delete deployment -n thorium operator
kubectl delete deployment -n kubegres-system kubegres-controller-manager
kubectl delete deployment.apps/cert-manager -n cert-manager
kubectl delete deployment.apps/cert-manager-cainjector -n cert-manager
kubectl delete deployment.apps/cert-manager-webhook -n cert-manager
kubectl delete statefulset.apps/elastic-operator -n elastic-system
```

Delete PVCs that remain after stateful resources have been deleted. Note: multiple PVCs may exist for multi-node k8s clusters that have deployed scaled up DBs.

```bash
# kubectl delete pvc -n redis redis-persistent-storage-claim
# kubectl delete pvc -n scylla data-scylla-us-east-1-us-east-1a-0
# kubectl delete pvc -n elastic-system elasticsearch-data-elastic-es-default-0
# kubectl delete pvc -n quickwit postgres-db-postgres-1-0
# kubectl delete pvc -n seaweedfs seaweedfs-data-seaweedfs-0
```
