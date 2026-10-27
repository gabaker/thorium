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

The settings Thorium depends on are the password and persistence to the data volume: an
append-only file (AOF) synced every second, so a crash loses at most about a second of writes,
plus RDB snapshots. This is a trimmed version of the chart's Redis conf (`redis-aof.conf`); see the
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
    # log every write to /data/appendonlydir and fsync it once a second
    appendonly yes
    appendfsync everysec
    # the password (requirepass), from the redis-auth Secret
    include /etc/redis/auth/auth.conf
```

This config is for a new, empty volume. To turn AOF on for a Redis that already holds data, see
[Switching an existing Redis to AOF](#switching-an-existing-redis-to-aof) first.

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

## Switching an existing Redis to AOF

Don't just change `appendonly no` to `yes` and restart a Redis that holds data. Redis 7 started
with `appendonly yes` and no AOF manifest (`appendonlydir/appendonly.aof.manifest`) starts empty: it ignores the
RDB file, and its next save overwrites that file with the empty data set. Turn AOF on while Redis
runs instead, so it writes the AOF from the data it has loaded. During and right after the switch
the volume holds the RDB file, its copy, the new AOF (about the size of the RDB file), and the
temporary file of the next RDB save (written in full before it replaces `dump2.rdb`), so check that
at least 3 times the size of `dump2.rdb`, plus some room for new writes, is free first:

```bash
# keep a copy of the RDB file until the AOF holds the data
kubectl -n redis exec redis-0 -- sh -c 'cp -p /data/dump2.rdb /data/dump2.rdb.pre-aof-$(date -u +%Y%m%dT%H%M%SZ)'
kubectl -n redis exec redis-0 -- env REDISCLI_AUTH="$REDIS_PASSWORD" redis-cli config set appendonly yes
# repeat until it shows aof_enabled:1, aof_rewrite_in_progress:0, aof_rewrite_scheduled:0, and
# aof_last_bgrewrite_status:ok
kubectl -n redis exec redis-0 -- env REDISCLI_AUTH="$REDIS_PASSWORD" redis-cli info persistence \
  | grep -E '^aof_(enabled|rewrite_in_progress|rewrite_scheduled|last_bgrewrite_status):'
kubectl -n redis exec redis-0 -- ls /data/appendonlydir
# appendonly.aof.1.base.rdb  appendonly.aof.1.incr.aof  appendonly.aof.manifest
```

Then set `appendonly yes` in `redis.conf` (`CONFIG REWRITE` can't, since the ConfigMap is
read-only) and restart Redis; it loads the AOF. Delete the `dump2.rdb.pre-aof-*` copy once you
no longer need it. If Redis later runs with `appendonly no` for a while, the AOF misses the writes
made meanwhile: move `/data/appendonlydir` aside and switch again the same way rather than
restarting with `yes`.

The chart's own Redis does this for you: its container runs `redis-start.sh` from the
`redis-conf` ConfigMap with the ConfigMap's `redis-aof.conf`. When the data directory holds
`dump2.rdb` and no AOF manifest, the script backs up `dump2.rdb`, starts Redis with AOF off, runs
the switch above, and logs each step. Later starts find the manifest and run `redis-server`
directly. If the volume lacks the space the switch needs, or the switch fails, Redis keeps serving
with AOF off and the next start tries again. If the pod is stopped during the switch, the script
turns AOF off first, so Redis saves `dump2.rdb` as it shuts down and the next start switches
again. The script finds the data directory, RDB file, AOF directory, and manifest the way Redis
does: from `dir`, `dbfilename`, `appenddirname`, and `appendfilename` in the conf and the files it
includes (the last one read wins), with a relative `dir` resolved from the directory Redis starts
in. Only the manifest Redis loads (`appendonly.aof.manifest`) counts: the temporary
`temp-appendonly.aof.manifest` a kill during the switch's last step can leave behind doesn't, so
that start switches again from `dump2.rdb`.

The switch needs free space on the data volume of 3 times the size of `dump2.rdb` (twice when an
identical `dump2.rdb.pre-aof-*` copy already exists) plus 64 MiB: the copy, the AOF base, and the
temporary file of the next RDB save. With the chart's default 16Gi volume and 512Mi memory limit
this always fits; a smaller volume or a larger limit may not.

### The start script's log lines

The script logs one line per step to the pod's log, each starting with `redis-start:` and a UTC
time, between Redis's own lines (`kubectl -n redis logs redis-0`). A start that finds the AOF already on disk logs none of them.

| Line | Meaning and what to do |
|------|------------------------|
| `appendonly is yes but <dir> has dump2.rdb and no AOF manifest ...; switching it to AOF` | The first start on an RDB-only volume; the switch begins. |
| `data dir <dir>, RDB file <path> (<size>), AOF dir <path>, backup <path>` | Where the data, the new AOF, and the copy of `dump2.rdb` are. |
| `<dir> has <bytes> free, the switch needs about <bytes>` | The space check passed. |
| `not enough space on <dir> to switch to AOF: needs <bytes>, has <bytes> free; ...` | No copy was taken and Redis serves with AOF off, saving `dump2.rdb` as before. Grow the PVC (if its storage class allows expansion) or free space on the volume, then restart the pod (`kubectl -n redis delete pod redis-0`); the next start tries again. |
| `backed up dump2.rdb to <path>` or `<path> already holds a copy of dump2.rdb` | The copy to fall back on until the AOF holds the data. |
| `could not back up dump2.rdb to <path>; ...` | The copy failed (usually a full volume); Redis serves with AOF off. Free space, then restart the pod. |
| `removed <dir>/temp-rewriteaof-*.aof, left by an interrupted AOF rewrite` | An earlier switch was cut short (the pod was killed); this one starts over. |
| `started Redis with AOF off (pid <n>)`, `loaded dump2.rdb (<n> keys); turning AOF on` | Redis loaded the data from `dump2.rdb`. |
| `AOF rewrite started ...`, `AOF rewrite running for <n>s: <n> of <n> keys written` | Redis writes the AOF; the second line repeats every 30 seconds for a long rewrite. |
| `AOF rewrite finished in <n>s; <dir>/appendonlydir holds <bytes>` and `AOF is on ...; <backup> (<size>) can be deleted once you no longer need it` | The switch succeeded. Delete the copy when you no longer want it (`kubectl -n redis exec redis-0 -- rm /data/dump2.rdb.pre-aof-<time>`). |
| `AOF rewrite failed after <n>s (aof_last_bgrewrite_status:err ...; <bytes> free on <dir>); the Redis log lines above give the cause` | Redis couldn't write the AOF. The Redis lines just above say why, such as `No space left on device`. Fix that, then restart the pod. |
| `CONFIG SET appendonly yes failed: ...` or `redis-cli could not authenticate: ...` | The script couldn't turn AOF on; check the error and the `redis-auth` Secret. |
| `switching to AOF failed; serving with AOF off (RDB saves only), the next start tries again` | Follows a failure above: Redis keeps serving, saving `dump2.rdb` as before. |
| `Redis exited during the switch to AOF; ...` | Redis stopped on its own during the switch; its lines above say why. The pod restarts and tries again. |
| `stopping Redis during the switch to AOF: turned AOF off so its shutdown saves dump2.rdb; ...` | The pod was stopped mid-switch. Nothing is lost; the next start switches again. |
| `moved <dir>/appendonlydir to <dir>/appendonlydir.stale-<time>: ...` | AOF was turned off just after the AOF was written, so the AOF lacks later writes and was set aside; the next start switches again from `dump2.rdb`. Delete the `.stale-` directory once Redis runs with AOF on. |
| `could not move <dir>/appendonlydir aside; ...` | Move it yourself before the pod starts again (for example by scaling the StatefulSet to 0 and moving it from a debug pod), or that start loads an AOF without the latest writes. |
| `stopping Redis`, `Redis exited (<status>)` | The pod is stopping; a status of 0 means Redis saved its data first. |

## Thorium settings

| Setting | Value |
|---------|-------|
| `redis.host` / `redis.port` | `redis.redis.svc.cluster.local` and `6379` (Helm: `operator.backends.redis.host` / `.port`) |
| `redis.password` | `$REDIS_PASSWORD` (Helm: `global.credentials.redisPassword`, required with an external Redis) |
| `redis.db` | Optional: the numbered Redis database Thorium selects (default `0`, the highest allowed is 2147483646; Redis Cluster only has `0`). It separates only Redis keys, so two deployments sharing a Scylla cluster still need their own `thorium.namespace`. Helm: `operator.cluster.config.redis.db` |
