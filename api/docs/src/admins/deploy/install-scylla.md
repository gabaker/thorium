# Scylla

This page sets up ScyllaDB with the Scylla operator for Thorium, the way the `thorium` chart
does it. Use it when you run Scylla yourself (`global.managed.scylla: false` with Helm, or the
operator without Helm). Scylla is Thorium's primary database; back it up with
[thoradm](../thoradm/thoradm.md). See [Bring Your Own Infrastructure](./infrastructure.md) for
what Thorium needs from it.

## 1) Install the Scylla operator

Install the Scylla operator (1.21.1) with the `infra-operators` chart, turning off the
operators you don't need:

```bash
helm install infra-operators oci://ghcr.io/cisagov/thorium/charts/infra-operators \
  --version $VERSION -n infra-operators --create-namespace --wait \
  --set eck-operator.enabled=false --set kubegres.enabled=false
```

The chart generates the operator's webhook certificate itself, so cert-manager isn't needed.
If you install the Scylla operator from ScyllaDB's own manifests or Helm chart instead, install
cert-manager first, as ScyllaDB's documentation describes.

Scylla needs `fs.aio-max-nr` raised on every node it runs on:

```bash
sudo sysctl -w fs.aio-max-nr=2097152
echo "fs.aio-max-nr=2097152" | sudo tee /etc/sysctl.d/99-scylla.conf
```

## 2) Create the Scylla config

Thorium logs in with a password, so Scylla must use `PasswordAuthenticator` and
`CassandraAuthorizer`. This is the config the chart uses (the Scylla operator passes each
node's own addresses and seeds on the command line, which take precedence over the address and
seed settings here):

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: scylla-config
  namespace: scylla
data:
  scylla.yaml: |
    num_tokens: 256
    commitlog_sync: periodic
    commitlog_sync_period_in_ms: 10000
    commitlog_segment_size_in_mb: 32
    seed_provider:
        - class_name: org.apache.cassandra.locator.SimpleSeedProvider
          parameters:
              - seeds: "127.0.0.1"
    listen_address: 0.0.0.0
    native_transport_port: 9042
    native_shard_aware_transport_port: 19042
    read_request_timeout_in_ms: 5000
    write_request_timeout_in_ms: 2000
    cas_contention_timeout_in_ms: 1000
    endpoint_snitch: SimpleSnitch
    rpc_address: localhost
    rpc_port: 9160
    api_port: 10000
    api_address: 127.0.0.1
    batch_size_warn_threshold_in_kb: 128
    batch_size_fail_threshold_in_kb: 1024
    authenticator: PasswordAuthenticator
    authorizer: CassandraAuthorizer
    partitioner: org.apache.cassandra.dht.Murmur3Partitioner
    commitlog_total_space_in_mb: -1
    murmur3_partitioner_ignore_msb_bits: 12
    force_schema_commit_log: true
    developer_mode: false
```

## 3) Deploy the Scylla cluster

```yaml
apiVersion: scylla.scylladb.com/v1
kind: ScyllaCluster
metadata:
  name: scylla
  namespace: scylla
spec:
  repository: docker.io/scylladb/scylla
  agentRepository: docker.io/scylladb/scylla-manager-agent
  version: 6.2.3
  agentVersion: 3.4.2
  # true skips Scylla's hardware checks so it runs on any disk; production needs false
  developerMode: false
  datacenter:
    name: us-east-1
    racks:
      - name: us-east-1a
        scyllaConfig: scylla-config
        members: 3
        storage:
          capacity: 32Gi
          # storageClassName: <your storage class>
        resources:
          limits:
            cpu: "4"
            memory: 16Gi
        # keep core dumps on the host; a hostPath volume isn't allowed by the baseline or
        # restricted Pod Security levels, so drop these in namespaces that enforce them
        volumes:
          - name: coredumpfs
            hostPath:
              path: /tmp/coredumps
        volumeMounts:
          - mountPath: /tmp/coredumps
            name: coredumpfs
```

```bash
kubectl create namespace scylla
kubectl apply -f scylla-config.yaml -f scylla-cluster.yaml
kubectl -n scylla get pods
# NAME                       READY   STATUS    RESTARTS   AGE
# scylla-us-east-1-us-east-1a-0   4/4     Running   0          10m
# ...
```

Production mode (`developerMode: false`) refuses to start on hosts that fail Scylla's checks;
provision Scylla nodes with XFS on NVMe storage and enough CPU and memory. Clients reach the
cluster through the `scylla-client` Service: `scylla-client.scylla.svc.cluster.local`.

## 4) Thorium's role

Thorium logs in as its own role, which must be a superuser because the API creates its keyspace
(with the replication factor in `scylla.replication`) and tables. Don't create the keyspace
yourself. Choose one of:

- **Let the operator create the role.** On a fresh Scylla the default `cassandra`/`cassandra`
  superuser exists, and the operator uses it to create Thorium's role with the configured
  username and password, then drops the `cassandra` role when `drop_default_role` is set. With
  Helm set `operator.cluster.bootstrap.scylla.enabled: true`; without Helm set
  `spec.bootstrap.scylla`. If the default superuser is gone, put a superuser's credentials in a
  Secret in the thorium namespace and name it in `bootstrap.scylla.adminSecret` (Helm) or
  `spec.bootstrap.scylla.admin_secret`.
- **Create the role yourself** as a superuser, then give Thorium its credentials:

  ```bash
  kubectl -n scylla exec -it scylla-us-east-1-us-east-1a-0 -c scylla -- \
    cqlsh -u cassandra -p cassandra \
    -e "CREATE ROLE thorium WITH PASSWORD = '<password>' AND LOGIN = true AND SUPERUSER = true;"
  ```

  Then replace the default superuser with one of your own (or drop `cassandra` once another
  superuser exists).

## Thorium settings

| Setting | Value |
|---------|-------|
| `scylla.nodes` | `["scylla-client.scylla.svc.cluster.local"]` (Helm: `operator.backends.scylla.nodes`) |
| `scylla.replication` | At most the number of Scylla nodes, for example `3` (Helm: `operator.backends.scylla.replication`) |
| `scylla.auth.username` / `password` | Thorium's role (Helm: `secrets.credentials.scyllaUsername` / `scyllaPassword`) |

With Helm and an external Scylla, the chart refuses to render unless `scyllaUsername` and
`scyllaPassword` are set or the Scylla bootstrap is on, since a generated password would never
match a role you created.
