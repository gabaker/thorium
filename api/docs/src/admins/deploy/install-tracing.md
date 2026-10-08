# Tracing (Quickwit and Jaeger)

This page sets up the tracing stack the `thorium` chart deploys when `global.quickwit.enabled`
is true: Quickwit stores Thorium's traces in S3 with its metastore in Postgres, and the Jaeger
query UI reads them back from Quickwit. Use it when you run tracing yourself, for example with
`global.quickwit.enabled: false` and an external collector, or with the operator without Helm.
Tracing is optional: without an external collector Thorium only logs locally. Any OTLP gRPC
collector works in place of Quickwit.

## 1) Create the metastore database

Quickwit keeps its metastore in Postgres. Any Postgres works; create a database for it
(the chart names it `quickwit-metastore`). To run Postgres in the cluster the way the chart does,
install Kubegres (1.19) with the `infra-operators` chart and create a `Kubegres` resource:

```bash
helm install infra-operators oci://ghcr.io/cisagov/thorium/charts/infra-operators \
  --version $VERSION -n infra-operators --create-namespace --wait \
  --set scylla-operator.enabled=false --set eck-operator.enabled=false
kubectl create namespace quickwit
POSTGRES_PASSWORD=$(openssl rand -hex 24)
kubectl -n quickwit create secret generic postgres-cluster-auth \
  --from-literal=superUserPassword="$POSTGRES_PASSWORD" \
  --from-literal=replicationUserPassword="$POSTGRES_PASSWORD"
```

```yaml
apiVersion: kubegres.reactive-tech.io/v1
kind: Kubegres
metadata:
  name: postgres
  namespace: quickwit
spec:
  replicas: 1
  image: docker.io/postgres:17
  database:
    size: 32Gi
    # storageClassName: <your storage class>
  env:
    - name: POSTGRES_PASSWORD
      valueFrom:
        secretKeyRef:
          name: postgres-cluster-auth
          key: superUserPassword
    - name: POSTGRES_REPLICATION_PASSWORD
      valueFrom:
        secretKeyRef:
          name: postgres-cluster-auth
          key: replicationUserPassword
```

Once Postgres is running, create the database on the primary instance:

```bash
PRIMARY=$(kubectl -n quickwit get pods -l app=postgres,replicationRole=primary -o name)
kubectl -n quickwit exec -it "$PRIMARY" -- \
  env PGPASSWORD="$POSTGRES_PASSWORD" createdb -U postgres quickwit-metastore
```

The metastore URI is then
`postgres://postgres:<password>@postgres.quickwit.svc.cluster.local:5432/quickwit-metastore`
(URL-encode the password if it holds special characters).

## 2) Create the bucket

Quickwit stores its indexes in an S3 bucket (`quickwit` by default) that must exist before it
starts. Nothing in Thorium creates it outside the chart's own SeaweedFS. Create it with your S3
service's tools, for example:

```bash
aws --endpoint-url "$S3_ENDPOINT" s3 mb s3://quickwit
```

## 3) Deploy Quickwit

Install the Quickwit Helm chart (0.8.17, image v0.8.2). Keep the credentials in a Secret that
Quickwit expands at startup, so none of them appear in its ConfigMap:

```bash
kubectl -n quickwit create secret generic quickwit-credentials \
  --from-literal=QW_METASTORE_URI="postgres://postgres:$POSTGRES_PASSWORD@postgres.quickwit.svc.cluster.local:5432/quickwit-metastore" \
  --from-literal=QW_S3_ENDPOINT="$S3_ENDPOINT" \
  --from-literal=AWS_ACCESS_KEY_ID="$S3_ACCESS_KEY" \
  --from-literal=AWS_SECRET_ACCESS_KEY="$S3_SECRET_KEY"
```

Save these values as `quickwit-values.yaml` (the same settings the `thorium` chart uses):

```yaml
# name the services quickwit-<component> whatever the release is called
fullnameOverride: quickwit
image:
  repository: docker.io/quickwit/quickwit
  pullPolicy: IfNotPresent
environmentFrom:
  - secretRef:
      name: quickwit-credentials
config:
  default_index_root_uri: s3://quickwit/quickwit-indexes
  metastore_uri: ${QW_METASTORE_URI}
  storage:
    s3:
      region: us-east-1
      endpoint: ${QW_S3_ENDPOINT}
      force_path_style_access: true
      access_key_id: ${AWS_ACCESS_KEY_ID}
      secret_access_key: ${AWS_SECRET_ACCESS_KEY}
      # flavor: minio   # for Rook/Ceph object stores
metastore:
  replicaCount: 1
searcher:
  replicaCount: 1
```

```bash
helm repo add quickwit https://helm.quickwit.io
helm repo update
helm install quickwit quickwit/quickwit --version 0.8.17 -n quickwit -f quickwit-values.yaml
kubectl -n quickwit get pods
```

Set `storage.s3.region` to the region your S3 service expects (`thorium-s3` for the Rook object
store from [Rook](./install-rook.md)).

## 4) Deploy Jaeger

The Jaeger query UI reads traces from the Quickwit searcher over gRPC:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: jaeger
  namespace: jaeger
spec:
  type: ClusterIP
  selector:
    app: jaeger
  ports:
    - name: jaeger
      port: 16686
      targetPort: 16686
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: jaeger
  namespace: jaeger
  labels:
    app: jaeger
spec:
  serviceName: jaeger
  replicas: 1
  selector:
    matchLabels:
      app: jaeger
  template:
    metadata:
      labels:
        app: jaeger
    spec:
      automountServiceAccountToken: false
      securityContext:
        runAsNonRoot: true
        runAsUser: 10001
        runAsGroup: 10001
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: jaeger
          image: docker.io/jaegertracing/jaeger-query:1.76.0
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: [ALL]
          env:
            - name: SPAN_STORAGE_TYPE
              value: grpc
            - name: GRPC_STORAGE_SERVER
              value: quickwit-searcher.quickwit.svc.cluster.local:7281
          resources:
            limits:
              cpu: "1"
              memory: 1Gi
```

```bash
kubectl create namespace jaeger
kubectl apply -f jaeger.yaml
kubectl -n jaeger port-forward svc/jaeger 16686   # Jaeger UI at http://localhost:16686
```

## Thorium settings

Thorium sends traces to the Quickwit indexer's OTLP gRPC port:

- **Helm:** set `operator.backends.tracing.grpcEndpoint` to
  `http://quickwit-indexer.quickwit.svc.cluster.local:7281`. With `global.quickwit.enabled: false`
  and no endpoint, Thorium sends no external traces.
- **Without Helm:** set it in the `ThoriumCluster`'s `spec.config` in map form (the operator
  doesn't accept YAML tags such as `!Grpc`):

  ```yaml
  thorium:
    tracing:
      external:
        Grpc:
          endpoint: http://quickwit-indexer.quickwit.svc.cluster.local:7281
          level: Info
      local:
        level: Info
  ```
