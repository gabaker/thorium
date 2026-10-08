# Redis

This page runs a single Redis instance for Thorium, the way the `thorium` chart does it. Use it
when you run Redis yourself (`global.managed.redis: false` with Helm, or the operator without
Helm). Redis holds Thorium's job queues, caches, and user data; back it up with
[thoradm](../thoradm/thoradm.md). See [Bring Your Own Infrastructure](./infrastructure.md) for
what Thorium needs from it.

Thorium only needs a Redis it can reach with a password (`AUTH` with the default user). Any
Redis 7 service works, including a managed one.

## 1) Create the password and config

Keep the password out of the main config so it never appears in a ConfigMap or on Redis's
command line: the config includes it from a Secret.

```bash
kubectl create namespace redis
REDIS_PASSWORD=$(openssl rand -hex 24)
printf 'requirepass "%s"\n' "$REDIS_PASSWORD" > auth.conf
kubectl -n redis create secret generic redis-auth --from-file=auth.conf
rm auth.conf
```

The settings Thorium depends on are the password and persistence to the data volume. This is a
trimmed version of the chart's `redis.conf`; see the
[upstream redis.conf](https://raw.githubusercontent.com/redis/redis/7.4/redis.conf) for every
setting:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: redis-conf
  namespace: redis
data:
  redis.conf: |
    bind 0.0.0.0
    protected-mode yes
    port 6379
    timeout 0
    tcp-keepalive 300
    daemonize no
    loglevel notice
    logfile ""
    databases 16
    # snapshot to /data after 900s with 1 change, 300s with 10, or 60s with 10000
    save 900 1
    save 300 10
    save 60 10000
    stop-writes-on-bgsave-error yes
    rdbcompression yes
    rdbchecksum yes
    dbfilename dump2.rdb
    dir /data
    appendonly no
    # the password (requirepass), from the redis-auth Secret
    include /etc/redis/auth/auth.conf
```

## 2) Deploy Redis

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: redis-persistent-storage-claim
  namespace: redis
spec:
  # storageClassName: <your storage class>
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 16Gi
---
apiVersion: v1
kind: Service
metadata:
  name: redis
  namespace: redis
spec:
  type: ClusterIP
  selector:
    app: redis
  ports:
    - name: redis
      port: 6379
      targetPort: 6379
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: redis
  namespace: redis
  labels:
    app: redis
spec:
  serviceName: redis
  replicas: 1
  selector:
    matchLabels:
      app: redis
  template:
    metadata:
      labels:
        app: redis
    spec:
      # the official image's redis user; fsGroup gives it the data volume
      securityContext:
        runAsNonRoot: true
        runAsUser: 999
        runAsGroup: 999
        fsGroup: 999
        fsGroupChangePolicy: OnRootMismatch
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: redis
          image: docker.io/redis:7.4.11
          command: ["redis-server", "/var/lib/redis/redis.conf"]
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: [ALL]
          ports:
            - containerPort: 6379
              name: redis
          readinessProbe:
            tcpSocket:
              port: 6379
            periodSeconds: 5
          resources:
            limits:
              cpu: "2"
              memory: 4Gi
          volumeMounts:
            - mountPath: /data
              name: redis-data
            - mountPath: /var/lib/redis/
              name: redis-conf
            - mountPath: /etc/redis/auth
              name: redis-auth
              readOnly: true
      volumes:
        - name: redis-conf
          configMap:
            name: redis-conf
        - name: redis-auth
          secret:
            secretName: redis-auth
            defaultMode: 0440
        - name: redis-data
          persistentVolumeClaim:
            claimName: redis-persistent-storage-claim
```

```bash
kubectl apply -f redis-conf.yaml -f redis.yaml
kubectl -n redis rollout status statefulset/redis
kubectl -n redis exec redis-0 -- redis-cli -a "$REDIS_PASSWORD" --no-auth-warning ping
# PONG
```

To see the exact manifests a chart release renders for its own Redis, render them from the
chart:

```bash
helm template thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION \
  -n thorium --set secrets.renderOnly=true --show-only charts/infra/templates/redis.yaml
```

## Thorium settings

| Setting | Value |
|---------|-------|
| `redis.host` / `redis.port` | `redis.redis.svc.cluster.local` and `6379` (Helm: `operator.backends.redis.host` / `.port`) |
| `redis.password` | `$REDIS_PASSWORD` (Helm: `secrets.credentials.redisPassword`, required with an external Redis) |
