# Rook

Rook runs Ceph on the cluster's own disks, giving Thorium a replicated S3 object store (the
Ceph RADOS Gateway) and a block storage class for the databases' volumes. This is what megathor
installs for multi-node clusters (`rook_enabled`). Use this page when your cluster has raw
disks you want to use for storage and no S3 service of its own; for a single node, the chart's
SeaweedFS (`global.managed.s3: true`) is simpler.

Rook needs at least three nodes with unused raw disks for the replicated settings below (or
`failureDomain: osd` with several disks on one node). Read Rook's own documentation for disk and
kernel requirements.

## 1) Install the Rook operator

```bash
helm repo add rook-release https://charts.rook.io/release
helm repo update
helm install rook-release rook-release/rook-ceph --version v1.18.1 \
  -n rook-ceph --create-namespace --set crds.enabled=true --wait
```

## 2) Create the Ceph cluster, block pool, and object store

Save these values as `rook-cluster-values.yaml`. They are the settings megathor uses: Ceph's
toolbox, a `ceph-block` default storage class, and an object store named `thorium-s3`.

```yaml
operatorNamespace: rook-ceph
toolbox:
  enabled: true
cephClusterSpec:
  mon:
    count: 3
    allowMultiplePerNode: false
  mgr:
    count: 2
    allowMultiplePerNode: false
  dashboard:
    enabled: false
  storage:
    useAllNodes: true
    useAllDevices: true
cephBlockPools:
  - name: rook-ceph-block
    spec:
      failureDomain: host
      replicated:
        size: 3
    storageClass:
      enabled: true
      name: ceph-block
      isDefault: true
      reclaimPolicy: Delete
      allowVolumeExpansion: true
      volumeBindingMode: Immediate
      parameters:
        imageFormat: "2"
        imageFeatures: layering
        csi.storage.k8s.io/provisioner-secret-name: rook-csi-rbd-provisioner
        csi.storage.k8s.io/provisioner-secret-namespace: rook-ceph
        csi.storage.k8s.io/controller-expand-secret-name: rook-csi-rbd-provisioner
        csi.storage.k8s.io/controller-expand-secret-namespace: rook-ceph
        csi.storage.k8s.io/controller-publish-secret-name: rook-csi-rbd-provisioner
        csi.storage.k8s.io/controller-publish-secret-namespace: rook-ceph
        csi.storage.k8s.io/node-stage-secret-name: rook-csi-rbd-node
        csi.storage.k8s.io/node-stage-secret-namespace: rook-ceph
        csi.storage.k8s.io/fstype: xfs
cephFileSystems: []
cephObjectStores:
  - name: thorium-s3
    spec:
      metadataPool:
        failureDomain: host
        replicated:
          size: 3
      dataPool:
        failureDomain: host
        replicated:
          size: 3
        parameters:
          bulk: "true"
      preservePoolsOnDelete: true
      gateway:
        port: 80
        instances: 1
        resources:
          requests:
            cpu: "1"
            memory: 4Gi
          limits:
            cpu: "4"
            memory: 16Gi
    storageClass:
      enabled: false
```

```bash
helm install rook-ceph-cluster rook-release/rook-ceph-cluster --version v1.18.1 \
  -n rook-ceph -f rook-cluster-values.yaml --wait
kubectl -n rook-ceph get pods -l app=rook-ceph-rgw
kubectl -n rook-ceph exec deploy/rook-ceph-tools -- ceph status
# health: HEALTH_OK
```

## 3) Create Thorium's S3 user

```bash
kubectl -n rook-ceph exec deploy/rook-ceph-tools -- \
  radosgw-admin user create --rgw-realm thorium-s3 --uid=thorium-s3-user --display-name='Thorium S3 User'
```

The output holds the user's `access_key` and `secret_key` (`radosgw-admin user info --rgw-realm
thorium-s3 --uid=thorium-s3-user` prints them again). Keep them private.

The operator creates Thorium's own buckets. If you deploy Quickwit, create its bucket
(`quickwit`) with these keys before installing; megathor does this from the `rook-ceph-tools`
pod, since the object store is only reachable inside the cluster.

## Thorium settings

| Setting | Value |
|---------|-------|
| `thorium.s3.endpoint` | `http://rook-ceph-rgw-thorium-s3.rook-ceph.svc.cluster.local` (Helm: `operator.backends.s3.endpoint`) |
| `thorium.s3.region` | `thorium-s3`: Rook names the Ceph zonegroup after the object store, and the gateway requires it as the bucket location constraint (Helm: `operator.backends.s3.region`) |
| `thorium.s3.use_path_style` | `true`: the object store is reached through one Service name, so bucket subdomains don't resolve (Helm: `operator.backends.s3.usePathStyle`) |
| `thorium.s3.access_key` / `secret_token` | The user's keys (Helm: `secrets.credentials.s3AccessKey` / `s3SecretToken`) |

With Helm, also set `global.managed.s3: false`, and with Quickwit set `global.quickwit.s3Endpoint`
to the same endpoint and `quickwit.config.storage.s3.region: thorium-s3` and
`quickwit.config.storage.s3.flavor: minio`. To use `ceph-block` for the databases' volumes, set
`infra.storageClass: ceph-block` (or leave it empty, since `ceph-block` is the default storage
class).
