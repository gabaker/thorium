# Thoradm

Thoradm is a command line tool similar to [Thorctl](../../getting_started/thorctl.md) that offers
functionality only available to Thorium admins. While some admin functions are available in
Thorctl (e.g. managing bans, notifications, and network policies), Thoradm focuses on the
infrastructure running Thorium: backups and restores, system settings, node provisioning,
censuses, user account cleanup, and the data migrations behind upgrade revisions.

## Getting Thoradm

The Thorium image ships Thoradm at `/app/thoradm`, so you can run it inside the cluster without
installing anything; the API pod already reaches every backend:

```bash
kubectl -n thorium exec deploy/api -- /app/thoradm migrate list
```

Every Thorium API also serves a Linux build at `<THORIUM_URL>/api/binaries/linux/x86-64/thoradm`.

## Config

Thoradm uses both the Thorctl config for user information (to verify admin status, for example)
and the cluster config, `thorium.yml`. The cluster config holds the credentials Thoradm needs to
read and restore data in Redis, S3, and Scylla. `thoradm migrate` needs neither.

The Thorium operator writes the full, rendered `thorium.yml` to the `thorium` Secret in the
thorium namespace (`<prefix>-thorium` with a namespace prefix). The `ThoriumCluster` itself holds
no credentials, so copy the config from that Secret:

```bash
kubectl -n thorium get secret thorium -o go-template='{{index .data "thorium.yml" | base64decode}}' > thorium.yml
chmod 600 thorium.yml
```

On a [minithor](../deploy/minithor.md) cluster, `minithor get-config` writes the same file to
`~/thorium.yml`, and `minithor get-config --local` writes `~/thorium.local.yml`, rewritten for the
ports `minithor expose --dev` forwards.

The config points at in-cluster service names, so run Thoradm where those names resolve and the
backends are reachable: inside the cluster (as above), or from a host with the backends
port-forwarded and the hosts in the config rewritten to match. By default Thoradm reads
`./thorium.yml`; give another path with `--cluster-conf/-c`:

```Bash
thoradm --cluster-conf <PATH-TO-THORIUM.YML> backup new
```

## Backup

Thoradm backs up Thorium's data (the Scylla tables, the Redis data, and the S3 objects of samples,
repos, comment attachments, results, and graphics) into a directory, checks a backup, and restores
it into a cluster. Elasticsearch is not backed up: it only holds search data derived from Scylla,
which the search streamer rebuilds by reindexing (see
[Reindexing Elasticsearch](../deploy/upgrades.md#reindexing-elasticsearch)). Take a backup before
upgrading Thorium or approving an upgrade step that changes data, so you can return to the
previous version or restore it if you need to. Also keep a copy of the `thorium-credentials`
Secret and your values files (see [Backups](../deploy/operate.md#backups)).

```Bash
thoradm backup -h
Backup a Thorium cluster

Usage: thoradm backup <COMMAND>

Commands:
  new       Take a new backup
  scrub     Scrub a backup for bitrot: the partition checks of `verify`, for every table
  verify    Check a backup directory, or a restored cluster, against the backup's manifest
  restore   Restore a backup to a Thorium cluster
  coverage  Print what a backup covers and what it excludes, from this binary, with no cluster or config
  help      Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

`backup new`, `backup restore`, and `backup verify --cluster` read the cluster config
(`--cluster-conf`) and the Thorctl config (`--ctl-conf`) described in [Config](#config);
`backup verify` of a backup directory, `backup scrub`, and `backup coverage` read neither. Global
flags such as `--workers` go before `backup`.

### What a Backup Holds

A backup holds every resource that can't be rebuilt from other data: every Scylla table of samples,
S3 ids, comments, results, tags, repos, commitishes, network policies, entities, associations,
notifications, and nodes (the materialized views are rebuilt from them); the users and their scoped
tokens, groups, network-policy sets, images, pipelines, and system settings held in Redis; and the
objects in the `files`, `repos`, `attachments`, `results`, and `graphics` buckets. The Taking
Backups guide lists every resource and the component that backs it up, generated from thoradm's
coverage registry, in [What Is Backed Up](./taking_backups.md#what-is-backed-up). To read the same
list from the installed binary, without a config file or a cluster, run `thoradm backup coverage`;
`--json` prints it as a JSON array with one object per resource, in the same order.

A backup doesn't hold runtime state, which is invalid after a restore, or data a restore rebuilds:

| Not backed up | Why |
|---|---|
| The `logs` and `events` Scylla tables | Stage logs of reactions and trigger events of the event handler; both expire by TTL |
| The Redis keys of reactions, jobs, workers, and services: `reactions:*`, `reactions_group:*`, `reaction_data:*`, `reaction_jobs:*`, `reaction_tags:*`, `reaction_cache:*`, `sub_reactions:*`, `generators:*`, `stage_logs:*`, `logs:*`, `job_queue:*`, `job_data:*`, `workers:*`, `worker_data:*`, `streams:*`, `event-handler:*`, `search-streamer:*`, `cursors:*`, and `image_ttc:*` | Runtime state, invalid after a restore |
| The OAuth keys `oauth:context:*`, `oauth:registration_sessions:*`, and `oauth:link_token:*` | Login contexts, registration sessions, and account-link tokens, which expire |
| `netpol_used_by:*` | Rebuilt by the Redis restore from the restored images' network policies |
| `census:*` | Counts rebuilt from the Scylla tables by the census that restore runs |
| `system_info` | Re-initialized by restore |
| `system_keys` | Written at setup and read by nothing |
| The `ephemeral` and `reaction_cache` buckets | Per-reaction scratch and cache files |

Because the Redis restore deletes every key of the namespace before it writes the backup's, a
restore leaves no reactions, jobs, or worker state behind, and Elastic's search indexes aren't part
of a backup (see [After a Restore](#after-a-restore)).

### Taking a Backup

```Bash
thoradm backup new --output /mnt/backups/thorium-2026-10-17
```

The [Taking Backups](./taking_backups.md) guide covers taking a backup in full: pausing the writers
first, the configs and access thoradm needs, the components and every flag of `backup new` with its
default, the output directory (a finished backup directory is never reused, so each backup needs a
new one), resuming an interrupted backup, the exit codes and `failures.jsonl`, and every file a
backup writes. A backup whose Redis records don't fit together (a group member or a user's group
that no longer exists, for example) fails its `redis` component until the records are repaired;
`backup new --prune-dangling` (off by default) drops the dangling user and group references
instead, lists each change in `pruned.jsonl`, and exits 2, and a restore of such a backup needs
`--accept-failures` (see [Pruning dangling
references](./taking_backups.md#pruning-dangling-references)). A backup whose rows name S3 objects
the cluster itself lacks (a broken link, such as an entity image deleted from the graphics bucket)
fails those objects by default; `backup new --accept-broken-links` (off by default) goes on past
each one, records it as a broken link in `failures.jsonl`, keeps it as the cluster holds it, and
exits 2, and a restore of a backup whose only records are accepted broken links needs no
`--accept-failures` (see [Accepting broken
links](./taking_backups.md#accepting-broken-links)).

### Checking a Backup

`thoradm backup verify --backup <BACKUP>` checks a backup against its manifest without touching the
cluster: every table's archives, partition checksums, and rows, every object, `redis.json`, and the
failures the backup recorded. It writes only its report. A backup without a `manifest.json` is
either interrupted (it holds `backup-in-progress.json`; finish it by running the same `backup new`
into it again) or legacy (taken by a thoradm older than manifests, holding neither file); verify
checks its archives but never passes it, since its completeness can't be checked.
[Checking a Backup](./taking_backups.md#checking-a-backup) in the Taking Backups guide lists every
check. The flags of `backup verify`:

| Flag | Default | What it does |
|---|---|---|
| `--backup`, `-b` | none (required) | The backup to check, or to check the cluster against |
| `--cluster` | off | Check the cluster in the cluster config against the backup instead of the backup directory (see [After a Restore](#after-a-restore)) |
| `--retries` | `5` | With `--cluster`: attempts per read of the cluster, including the first, before its failure is recorded |
| `--report-dir` | `<backup>/reports/verify-<time>/` | Where to write the report (`verify.json`) and `failures.jsonl` |
| `--abort-on-failure` | off | Stop at the first failed check: record it and exit 1 |

`thoradm backup scrub --backup <BACKUP>` runs only verify's partition checks, for every table: it
re-hashes each partition and compares it with the checksum stored when the backup was taken, so it
finds bitrot in a backup with or without a manifest. It writes its failures to
`reports/scrub-<time>/failures.jsonl` in the backup. A mismatch means the backup is corrupt, and
restoring it would restore damaged data; restore runs verify first and refuses such a backup. The
flags of `backup scrub`:

| Flag | Default | What it does |
|---|---|---|
| `--backup`, `-b` | none (required) | The backup to scrub |
| `--abort-on-failure` | off | Stop at the first partition that fails its check: record it and exit 1 |

## Restoring a Backup

```Bash
thoradm backup restore --backup <BACKUP>
```

A restore writes the backup into the cluster the cluster config names: it replaces the namespace's
Redis data with `redis.json`, writes every table's rows, and uploads every object to its original
key. Like a backup, it can take hours for a large backup. Before it writes anything it runs a
preflight that verifies the backup and checks the target; [How Restore Uses a
Backup](./taking_backups.md#how-restore-uses-a-backup) in the Taking Backups guide lists every check
and the flag that overrides it.

### Restore Requirements

- **An empty target.** A plain restore refuses a target whose backed-up tables already hold a row,
  and changes nothing. Restore into such a target only with `--merge` or `--wipe <namespace>` (see
  [Merge or Wipe](#merge-or-wipe)). Only the Scylla tables are checked: Redis isn't, since every
  restore replaces the namespace's users, groups, images, and pipelines, and buckets aren't, so a
  target whose tables are empty restores without a flag even when its buckets still hold objects
  (a restore only overwrites an object at the same key). A newly deployed cluster isn't empty: its
  operator registers each node in the `nodes` table, so restore a backup into a new deployment with
  `--wipe <namespace>`.
- **A finished backup.** An interrupted backup (`backup-in-progress.json` without `manifest.json`) is
  refused, with no override: finish it by running the same `backup new` into it again.
- **`--legacy` for a legacy backup.** A backup with neither `manifest.json` nor
  `backup-in-progress.json` was taken by a thoradm older than manifests. It restores only with
  `--legacy`; its completeness can't be checked, so the run warns and the summary says it was
  restored as legacy. `--legacy` is refused for any other backup, including one whose
  `manifest.json` was removed.
- **`--accept-failures` for a backup with recorded failures.** A backup that exited 2 is missing the
  objects its `failures.jsonl` lists, or, taken with `--prune-dangling`, had the references its
  `pruned.jsonl` lists dropped. It restores only with `--accept-failures`, a plain switch with no
  value; the restore warns, and its summary lists every object the backup is missing (the pruned
  references are in `pruned.jsonl`, and the restore writes the Redis state without them). The
  restore's own exit code follows only its own failures. A backup whose only records are broken links
  accepted with `backup new --accept-broken-links` needs no flag: the restore logs them as known and
  its summary lists them as `broken_links`, and the restored rows keep the links as the backup held
  them.
- **A thoradm that reads the backup's format.** A thoradm reads every archive format up to its own,
  so restore with a thoradm at least as new as the one that took the backup ([Archive
  formats](./taking_backups.md#archive-formats)). A format-2 backup must only be restored with a
  thoradm that supports format 2: a thoradm that writes manifests but reads only format 1 refuses it
  before it writes anything, naming the version that wrote it, while a thoradm older than manifests
  (the thoradm of earlier Thorium releases) does not refuse it, replaces the namespace's Redis data,
  and then fails on the Scylla data.
- **The source's `secret_key`.** Password hashes are made with the API's `thorium.secret_key`, so
  configure the target's API with the source cluster's `secret_key`, or every password login fails
  after the restore. Thoradm doesn't check it.
- **The target's own users and nodes, for another deployment's backup.** The restore replaces the
  users the operator manages (`thorium-operator`, `thorium`, and `thorium-kaboom`) with the source's,
  whose passwords the target's `thorium-operator-pass`, `thorium-pass`, and `thorium-kaboom-pass`
  Secrets don't hold, and the `nodes` table with the source's nodes. Until that is fixed the operator
  can't log in (its phase is `Error`), the components fail with `401`, and the scaler then stops
  with `No clusters are available to schedule on`. After the restore, set each of the three users'
  passwords to its Secret's value as an admin (`PATCH /api/users/user/<user>` with
  `{"password": ...}`), and let the operator reconcile, which registers this cluster's nodes, before
  you deploy a newer release: an operator waiting on an upgrade registers no nodes, so register them
  yourself then (`POST /api/system/nodes/` with the cluster alias, node name, and zero resources).
- **An admin in the Thorctl config.** The Redis data is restored through the API, so the Thorctl
  config must hold an admin's token for the target's API.
- **A thoradm of the API's version.** A restore of a backup holding `redis.json`, and every
  `--wipe` restore, is refused (check `version`) when the API reports a Thorium version other than
  thoradm's, patch included, or none, since each side would read the Redis data with its own version
  of it. `--allow-version-mismatch` lets the restore go ahead: it warns, and the summary's
  `warnings` names both versions. The check compares version numbers only, so it tells thoradm and
  the API apart only when they were released with different versions. An API older than this
  thoradm's release clears Redis for a restore with `FLUSHDB`: every key in its Redis database,
  other namespaces' included, not only the namespace's keys. See [Run thoradm at the API's
  version](./taking_backups.md#run-thoradm-at-the-apis-version).

What isn't backed up isn't restored (see [What a Backup Holds](#what-a-backup-holds)): the Redis
restore deletes the namespace's reactions, jobs, and worker state, and the excluded Scylla tables
and buckets keep whatever the target held.

### Merge or Wipe

When the target already holds data, choose what happens to it. `--merge` and `--wipe` exclude each
other.

**`--wipe <namespace>`** rolls the namespace's tables back to the backup. It truncates every
backed-up table the backup holds (after saving and dropping its views, which the restore
recreates). It deletes nothing from object storage and sends no S3 request at all, so it never
lists a bucket: every bucket is left as it is. It keeps what the backup doesn't hold: the stage logs
and events tables and the buckets. After the restore, the tables and Redis equal the backup and
every object the backup's rows name is present (an object at the same key is overwritten), but an
object no restored row names, such as an object whose sample was deleted after the backup, may
remain in a bucket. **Thoradm doesn't reclaim that space: the admin cleans up orphaned objects**,
if they matter, with the object store's own tools. Redis needs nothing extra, since every restore
deletes every key of the namespace before writing the backup's (keys of other namespaces sharing the
Redis database are left alone, unless `--redis-flush-all` is given). The value is required: a bare
`--wipe` is a usage error (exit 1), and so is a value that breaks the namespace rule a Thorium
config follows (lowercase ASCII letters `a-z`, digits and underscores, starting with a lowercase
letter, at most 48 characters). A deployment whose namespace has uppercase letters must rename it
before the API starts, and its Redis state under the old prefix isn't carried over (see [Deploy
Thorium](../deploy/deploy-thorium.md)).

Nothing is deleted until every other check passes (a damaged backup, or one with recorded failures
and no `--accept-failures`, never causes a wipe). The wipe then refuses (check `wipe`) when:

- the namespace given to `--wipe` isn't, exactly, the one the cluster config names;
- the API the Thorctl config points at serves another namespace, or doesn't report one (an API
  older than this thoradm); upgrade the API, or restore without `--wipe`;
- the namespace breaks the namespace rule (lowercase letters, digits, `_`), names a Scylla system
  keyspace, or holds none of the tables a backup holds.

Bucket names aren't checked: the wipe deletes no object, so no bucket can be emptied by mistake.

The wipe shows its plan first: each table with an approximate partition count (`empty` for a table
with no rows, and `unknown` for rows Scylla hasn't estimated yet, as after recent writes), each
backed-up bucket named as left untouched (with no object counts), and what is kept. On a terminal
you confirm by typing the namespace, even with `--confirm`; without a terminal, `--wipe <namespace>
--confirm` is the unattended form and the plan goes to the log. When the backed-up tables are
already empty there is nothing to wipe, and only the normal confirmation applies: no `wipe.json` is
written and the summary's `wipe` is empty. Each table is logged at WARN as it is wiped, and
`wipe.json` in the report is rewritten after each, so it always says what is gone; it lists the
buckets left untouched as `buckets_left`.

Scylla's `auto_snapshot` (on by default) keeps a snapshot of every truncated table on each of its
nodes. These truncate snapshots hold the wiped data and its disk: the wipe frees no Scylla disk
until they are cleared. They aren't a complete copy of the wiped rows, though: on a node with more
than one shard, rows written shortly before the wipe can be missing from them, so keep the backup
you restore as the copy of that data. Thoradm never clears them. It warns about them twice:

- **The plan** says whether `auto_snapshot` is on (read from each node's `system.config`), names the
  snapshot each wiped table will leave with the size Scylla estimates for the table (or "size
  unknown" when Scylla hasn't estimated it yet, as after recent writes), lists the snapshots the
  wiped tables already have, with their size and node, and prints the command that clears them.
  With `auto_snapshot` off it says the truncates leave no snapshots, so the wiped rows can't be
  recovered from Scylla.
- **The final report** lists each snapshot the wipe left, by name, table, and node, with its size
  on disk (from each node's `system.snapshots`, which `nodetool listsnapshots` also shows), and
  the same command; `wipe.json` and the summary's `wipe` record them as `snapshots`, and the
  earlier ones as `existing_snapshots`.

Once `thoradm backup verify --cluster` passes, run `nodetool clearsnapshot <namespace>` on every
Scylla node to free that disk. It clears every snapshot of the keyspace, including any you took
yourself, so copy those elsewhere first.

A wipe that fails partway (a table that can't be truncated after `--retries` attempts) stops the
restore with exit 1 before Redis, with or without `--abort-on-failure`: a `wipe` record names the
table, `wipe.json` has no `finished` time, and nothing has been restored. A plain re-run is then
refused by the empty-target check, since the tables not yet truncated still hold rows. Fix the cause and run the same `--wipe <namespace>` restore again: it finds the unfinished
`wipe.json` (the newest under `reports/restore-*/`, or in or beside the `--report-dir`), says it is
resuming it, records it as `resumed_from`, wipes what is left, takes the views the first run dropped
from that run's `views.cql`, and restores. When the first run left the target empty, the re-run has
nothing to wipe, so it writes no `wipe.json` and records no `resumed_from`; it still takes the
dropped views from the first run's `views.cql`.

**`--merge`** adds the backup's data to a namespace whose own data must stay. Use it only for that:
rows and objects created after the backup survive, rows and objects deleted after the backup come
back, and on a key both hold the backup's values win. Redis is still replaced by the backup's, so
users, groups, images, and pipelines created after the backup are gone. To roll a namespace back,
use `--wipe <namespace>` instead. A backup's `graphics` holds only the icons and images its own
records name, and needs the `entities` and `redis` components; see
[Choosing components](./taking_backups.md#choosing-components) in the Taking Backups guide.

### Running a Restore

Pause users and every writer (the scaler, event handler, search streamer, and reactors) before a
restore and keep them paused until [After a Restore](#after-a-restore) is done: the restore drops
each table's views while it loads the table, so the API's view-backed routes fail until they are
back, and data written during the restore mixes with the backup's. The API itself stays up, since
the Redis data is restored through it.

The restore asks for confirmation on a terminal; without one, `--confirm` is required. The
confirmation, or the log under `--confirm`, names the namespace, Redis host, Scylla nodes, and buckets;
the users and groups the Redis restore replaces; that the Redis restore first deletes every key of
the namespace, its job queues, reaction state, and census streams included (with
`--redis-flush-all`, that every key in the database will be deleted, other namespaces' keys
included); what `--merge`,
`--legacy`, and `--accept-failures` accepted; and everything the backup doesn't hold, which the
Redis restore deletes from Redis or leaves stale in Scylla and S3.

```Bash
thoradm backup restore --backup /mnt/backups/thorium-2026-10-17 --wipe thorium --confirm
```

The flags of `backup restore`:

| Flag | Default | What it does |
|---|---|---|
| `--backup`, `-b` | none (required) | The backup to restore; any relative or absolute path |
| `--confirm` | off | Confirm without asking; required when no terminal is attached. A `--wipe` restore on a terminal still asks for the namespace to be typed |
| `--merge` | off | Restore into a target whose backed-up tables already hold data, keeping what is there (see [Merge or Wipe](#merge-or-wipe)) |
| `--wipe <namespace>` | off | Truncate the target's backed-up tables first (no object is deleted, so orphaned objects may remain), then restore; the value must be the namespace the cluster config names. Excludes `--merge` |
| `--redis-flush-all` | off | Have the API clear Redis with `FLUSHDB ASYNC` of its database instead of deleting only the namespace's keys: every key in the database is deleted, other namespaces' included, while Redis's other databases are untouched (it is not `FLUSHALL`). For large restores into a dedicated database (see [Large restores](./taking_backups.md#large-restores-flushing-the-redis-database) in the Taking Backups guide) |
| `--legacy` | off | Restore a legacy backup, one with neither `manifest.json` nor `backup-in-progress.json` |
| `--accept-failures` | off | Restore a backup whose manifest records failed or skipped objects, or references `--prune-dangling` dropped |
| `--allow-version-mismatch` | off | Restore a backup holding `redis.json`, or wipe, even when the API reports a Thorium version other than thoradm's, or none; the mismatch is logged at WARN and listed in the summary's `warnings` |
| `--report-dir` | `<backup>/reports/restore-<time>/` | Where to write the report |
| `--retries` | `5` | Attempts per object, including the first, before its failure is recorded; a transient error (a 5xx, a throttling response, a reset connection) is retried after a growing delay |
| `--s3-concurrency` | `64` | The most S3 uploads in flight at once, across every S3 component (1 to 65,536) |
| `--abort-on-failure` | off | Stop at the first object that fails: record it and exit 1, leaving a partly restored target |
| `--workers`, `-w` | `10` | A global flag, given before `backup`: the number of workers (at least 1) |

The restore runs in this order:

1. **The preflight** verifies the backup and checks the target and the flags; a refusal changes
   nothing.
2. **The wipe**, with `--wipe <namespace>` (see [Merge or Wipe](#merge-or-wipe)).
3. **Redis.** The API restores Redis in steps: wipe (every key of the namespace, or with
   `--redis-flush-all` a `FLUSHDB ASYNC` of the database), init, settings, users, scoped tokens,
   groups, images, pipelines, and network-policy sets.
4. **The tables.** Before it loads the first table, restore saves the statement of every
   materialized view of the tables it restores to `views.cql` in the report, from Scylla's own
   `DESCRIBE`. It drops each table's views just before loading the table, and once every table is
   loaded it recreates them from the saved statements and waits for them to build: no API restart
   is needed. Each node is written as registered, with no heartbeat: its workers were Redis state,
   so each node must check in again before the scheduler counts it.
5. **The objects**, each uploaded to its original key, addressing buckets the way
   `thorium.s3.use_path_style` says. A failed upload is aborted (no parts are left in the bucket),
   retried if its error is transient, and recorded if it still fails.
6. **The census** of every restored table, and of every table when the backup holds `redis.json`
   (whose restore deletes every census stream), so the listings work at once.
7. **The summary**, logged and written as `summary.json`.

Restored notifications keep the TTL they had left when the backup ran, and permanent ones stay
permanent; from a format-1 or legacy backup, which keeps no TTLs, they take the table's default TTL
and the summary warns of it.

**Flushing the Redis Database.** The default wipe walks every key in the Redis database to find
the namespace's, about 1.5 s per million keys in the database whichever namespace they belong to
(about 14 minutes for 570 million small keys, roughly 100 GB). For large restores,
`--redis-flush-all` (off by default) has the API run `FLUSHDB ASYNC` on its database instead, about
0.1 s per million keys. It deletes every key in the database the API uses (database 0), other
namespaces' and other applications' keys included, but not Redis's other databases (it is not
`FLUSHALL`), so use it only when the database is dedicated to this Thorium. The validation still
runs first (a refused backup leaves Redis untouched) and the steps after the flush are the same.
The flush returns at once and Redis frees the memory in the background, so its memory use falls
over the following moments. Pause every writer first, as for any restore: a key written during the
flush may be deleted with the rest. The [Taking Backups
guide](./taking_backups.md#large-restores-flushing-the-redis-database) covers it in full.

**Views.** Restore waits for the recreated views to build with no time limit (a large table's view
can take long). It logs each view as it finishes (`View <keyspace>.<view> built after <n>s`) and,
every 30 seconds, which views are built and which are still building, with how many of the nodes
that reported have finished each one. Each view is recreated on its own: one that can't be is recorded (operation
`post-restore`, the view as its key) and the others are recreated all the same, and a view another
client (an API pod) created first counts as recreated when its definition matches the saved
statement (otherwise it is recorded). A restore that is interrupted leaves its `views.cql`; running
the restore again takes any view it finds missing from the newest earlier `views.cql` that holds its
statement for this namespace's keyspace (under `reports/restore-*/`, or in or beside the
`--report-dir`; files that restores of the same backup into other namespaces left hold their
keyspaces' statements and are skipped), and the preflight refuses (check `views`) when no such file
holds it. Then replay an earlier `views.cql` with `cqlsh -f` after checking it, or restart one API
pod, whose schema setup creates missing views.

**A failed Redis restore.** A Redis step that fails stops the Redis restore with a `redis` record
whose error names the step and what the steps before it wrote, and the rest of the restore carries
on. Once the cause is fixed, run the same restore again, adding `--merge` when the first run
restored tables or objects, which the empty-target check otherwise refuses. When the failed step is
the wipe, init, settings, or users step, the namespace may hold no users, so the admin in the
Thorctl config can't log in (the rerun's Redis restore fails with 401) and the summary of a run
whose Redis restore failed warns of it. Bootstrap an admin first with the API's
`thorium.secret_key`:

```bash
curl -X POST <api>/api/users/ -H "secret-key: <secret_key>" -H "content-type: application/json" \
  -d '{"username": "<name>", "password": "<password>", "email": "<email>", "role": "Admin",
       "local": true, "skip_verification": true}'
```

Point the Thorctl config at that admin (`thorctl login`) and run the restore again. The Redis
restore replaces the bootstrapped admin with the backup's users, whose logins work again afterwards.

### After a Restore

1. **Check the cluster against the backup.** With every writer still paused, run
   `thoradm backup verify --cluster --backup <BACKUP>`. The Redis check goes through the API in the
   Thorctl config, so log in (`thorctl login`) as an account the backup holds: the restore replaced
   the target's users, and the token you restored with may belong to a user the backup doesn't hold.
2. **Rebuild the search indexes.** Elastic isn't restored. Run the search-streamer once with
   `--reindex` to delete the search indexes and rebuild them from the restored data.
3. **Clear Scylla's truncate snapshots** after a `--wipe` restore, once the check passes:
   `nodetool clearsnapshot <namespace>` on every Scylla node frees the disk the wiped data still
   holds (the restore's final report lists the snapshots and prints the command).
4. **Let the operator log in again** after restoring a backup of another deployment. The Redis
   restore replaces the operator's own `thorium-operator` user with the backup's, whose password is
   the source deployment's. The operator logs in with the password in its `thorium-operator-pass`
   Secret, so on any other deployment its reconcile fails with `401 Unauthorized` (phase `Error`)
   and the components you paused stay at zero. Set the user's password to the Secret's value as an
   admin the backup holds, or one bootstrapped with the `secret_key` as above; the operator then
   logs in at its next reconcile and resets the other system users' passwords itself:

   ```bash
   PASS=$(kubectl -n thorium get secret thorium-operator-pass -o jsonpath='{.data.thorium-operator}' | base64 -d)
   curl -X PATCH <api>/api/users/user/thorium-operator -H "Authorization: token <admin token, base64>" \
     -H "content-type: application/json" -d "{\"password\": \"$PASS\"}"
   ```

5. **Resume the writers and users.** Nodes check in again on their own.

`verify --cluster` writes its report like `verify` and checks:

| Check | Fails when |
|---|---|
| Rows | A backed-up table holds another number of rows in a token range than the backup read from it (the ranges and counts come from the manifest) |
| Objects | An object the backup's indexes list is missing from its bucket, or has another size |
| Redis | A user, scoped token, group, network-policy set, image, or pipeline the API serves differs from `redis.json`, or is in only one of them, or the settings differ; the record names the fields that differ, never their values. Sets (group members, an image's network policies and child filters, a group's network policies, the host path whitelist) compare in any order |
| Views | A view the registry lists for a backed-up table doesn't exist or hasn't finished building |
| Census | A census stream lacks a bucket that a table with a census holds: every table the backup holds, and, when the backup holds `redis.json`, every such table in the target, since the Redis restore deletes every census stream |

It needs a backup with a manifest and pauses for nothing: rows written since the restore count as
differences, as do a `--merge` restore's rows the backup doesn't hold. When the API reports a
Thorium version other than thoradm's, or none, `verify --cluster` logs the mismatch at WARN and
checks anyway, so a Redis difference may come from the versions rather than the restore; it takes
no `--allow-version-mismatch`.

**An interrupted restore** (killed, crashed, or stopped by `--abort-on-failure`) leaves a partly
restored target. Run the restore again with `--merge`, which writes everything again over what the
first run wrote, or with the same `--wipe <namespace>` if the interrupted run used it, which wipes
and restores again (a failed wipe resumes, as [Merge or Wipe](#merge-or-wipe) describes).

## Reading the Results

`backup new`, `restore`, `verify`, and `scrub` end with one of these exit codes, which their
`--help` lists too:

| Exit code | Meaning |
|---|---|
| 0 | The run finished and nothing was recorded: every object was backed up, restored, or checked |
| 2 | The run finished, but objects failed or were skipped, each listed in the run's `failures.jsonl`; for `backup new`, also when `--accept-broken-links` accepted broken links, each listed there as a `broken-link` record, or `--prune-dangling` dropped references, each listed in `pruned.jsonl` |
| 1 | The run stopped early: `--abort-on-failure` stopped it at its first failure, or a preflight check, a usage or config error, or a failure that isn't per object (a component that couldn't run, a wipe that failed) stopped it |
| 101 | Thoradm crashed (a panic); report it with the stderr output |

A run goes to completion past every failed object by default; no count or threshold of failures
stops it. `--abort-on-failure` is the only way to stop at the first:

| Command | Without `--abort-on-failure` | With `--abort-on-failure` | After fixing the cause |
|---|---|---|---|
| `backup new` | Every failure is recorded and the backup finishes (exit 2) | Stops at the first failure (exit 1) and leaves an interrupted backup | Exit 2: take a new backup into a new directory. Exit 1: run the same command into the same directory to resume |
| `backup restore` | Every object that can't be written is recorded and the restore finishes (exit 2) | Stops at the first failure (exit 1), leaving a partly restored target | Run the restore again with `--merge`, or the same `--wipe <namespace>` (see [After a Restore](#after-a-restore)) |
| `backup verify` | Every check runs and each failure is recorded (exit 2) | Stops at the first failed check (exit 1) | A backup that fails verify is damaged: take a new backup |
| `backup scrub` | Every partition is checked and each failure is recorded (exit 2) | Stops at the first failed partition (exit 1) | A partition that fails is corrupt: take a new backup |

The last line on stderr says how the run ended, and each failure is logged on stderr as it happens.
Each run writes its failures, one JSON object per line, to a `failures.jsonl` whose fields
[Reading `failures.jsonl`](./taking_backups.md#reading-failuresjsonl) describes:

| Command | Where it writes its report |
|---|---|
| `backup new` | `failures.jsonl` in the backup directory; a resumed run moves the interrupted run's to `reports/backup-<time>/` |
| `backup restore` | `reports/restore-<time>/` in the backup, or `--report-dir`: `failures.jsonl`, `verify.json` (the preflight's verify), `views.cql`, `wipe.json` (with `--wipe`), and `summary.json` |
| `backup verify` | `reports/verify-<time>/` in the backup, or `--report-dir`: `failures.jsonl` and `verify.json` |
| `backup scrub` | `reports/scrub-<time>/failures.jsonl` in the backup |

A restore's `summary.json` holds what each component restored, skipped, and failed, the failures,
whether the backup was restored as legacy, what `--merge` merged into or `--wipe` truncated, the
objects the backup is missing, the views recreated, the census rebuilt, and its warnings; it is
written only when the restore gets past the preflight (and a `--wipe` restore past its wipe).

## System Settings

Thoradm also provides functionality to modify dynamic Thorium system settings that aren't contained in the
cluster config file described above. By "dynamic", we mean settings that can be modified and take effect while
Thorium is running without a system restart.

```Bash
thoradm settings -h
Edit Thorium system settings

Usage: thoradm settings <COMMAND>

Commands:
  get     Print the current Thorium system settings
  update  Update Thorium system settings
  reset   Reset Thorium system settings to default
  scan    Run a manual consistency scan based on the current Thorium system settings
  help    Print this message or the help of the given subcommand(s)
```

### Viewing System Settings

You can view system settings with the following command:

```Bash
thoradm settings get
```

The output will look similar to the following:

```JSON
{
  "reserved_cpu": 50000,
  "reserved_memory": 524288,
  "reserved_storage": 131072,
  "fairshare_cpu": 100000,
  "fairshare_memory": 102400,
  "fairshare_storage": 102400,
  "host_path_whitelist": [],
  "allow_unrestricted_host_paths": false
}
```

### Updating System Settings

You can update system settings with the following command:

```Bash
thoradm settings update [OPTIONS]
```

At least one option must be provided. You can view the commands help documentation to see a list of
settings you can update.

### Reset System Settings

You can restore all system settings to their defaults with the following command:

```Bash
thoradm settings reset
```

### Consistency Scan

Thorium will attempt to remain consistent with system settings as they are updated without a restart.
It does this by running a consistency scan over all pertinent data in Thorium and updating that data
if needed. There may be instances were data is manually modified by an admin or added such that they
are no longer consistent. For example, an admin adds a host path volume mount with a path that is not
on the host path whitelist, resulting in an image with an invalid configuration that is not properly
banned.

You can manually run a consistency scan with the following command:

```Bash
thoradm settings scan
```

## Provision Thorium Resources

Thoradm can also provision resources for Thorium. Currently, nodes are the only resource available to be
provisioned by Thoradm.

```Bash
thoradm provision -h
Provision Thorium resources including nodes

Usage: thoradm provision <COMMAND>

Commands:
  node  Provision k8s or baremetal servers
  help  Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

### Provision a Node

Run on a Kubernetes node, this prepares the node for Thorium jobs: it creates `/opt/thorium`,
downloads the Thorium agent from the API (authenticating with a Thorium keys file), and installs
the tracing config next to it.

```Bash
thoradm provision node --keys <PATH-TO-KEYS-FILE>
```

The Thorium operator does this for you: its node provision pods run this command on every node
the k8s scaler schedules on and label the node for Thorium. Provisioning bare metal servers
(`--baremetal`) is not supported yet.

## Census

A census counts Thorium's data in Scylla (tags, files, repos, commitishes, entities, and
associations) and rebuilds the counts Thorium caches in Redis for listing that data. A restore
takes one itself; take a new one when listings look wrong:

```Bash
thoradm census new [all|tags|tags-case-insensitive|files|repos|commitishes|entities|associations]...
```

With no kind it takes a census of everything. `--dry-run/-d` only reports the counts without
saving them, and `--multiplier/-m` (default 100) sets the chunk multiplier used with the worker
count (`--workers`).

## Users

Find and fix accounts that share an email address (system accounts may share the email given
with `--system-email/-s`, default `thorium`):

```Bash
# list non-system accounts that share an email address
thoradm users scan
# interactively assign unique emails; --dry-run prints the planned changes, -y skips the prompt
thoradm users dedupe [--dry-run] [-y]
```

## Migrate

`thoradm migrate` lists the [upgrade revisions](../deploy/upgrades.md) of the build you run and the
steps between them. It reads no config, so it runs anywhere, including inside the cluster with
`kubectl -n thorium exec deploy/api -- /app/thoradm migrate list`.

```Bash
thoradm migrate list
thoradm migrate plan --from <revision> [--to <revision>]
thoradm migrate run <migration id>
thoradm migrate verify <migration id>
```

`list` prints every revision and its steps:

```
2026-10-v01 (released with Thorium 1.8.1): Thorium deployed by the minithor or megathor scripts before the Helm charts (the baseline every converted cluster starts from)
2026-10-v02 (released with Thorium 1.8.1): Thorium deployed by the Helm charts with the chart-managed operator
  - scylla-role [Infra]: Check that Thorium's Scylla role logs in with the configured credentials
  - elastic-identity [Infra]: Check that Thorium's Elastic user authenticates and holds the index privileges Thorium needs
  - elastic-reindex-keyword-mappings [Data, needs approval + backup, scales down search-streamer, migration elastic-keyword-mappings (not implemented; manual procedure)]: Reindex Elastic indexes created without mappings, whose group field is not a keyword (megathor deployments before the indexes were left to the search-streamer); skipped when every index is already correct
  - finalize [Config]: Record the new revision
```

`plan` prints the steps from one revision to another (the latest by default), for example
`thoradm migrate plan --from 2026-10-v01`. Find a cluster's revision in the `Revision` column of
`kubectl -n thorium get thoriumcluster`.

`run` and `verify` take a migration id from `list`. No migration is implemented in Thoradm yet:
both print the manual procedure for the migration and exit with code 3, which scripts can tell
apart from a failure (exit code 1, such as for an unknown id).
