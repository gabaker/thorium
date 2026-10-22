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
read and restore data in Redis, S3, and Scylla. `thoradm migrate list` and `plan` need neither;
every other `thoradm migrate` subcommand needs the cluster config but not the Thorctl config.

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
objects in the `files`, `repos`, `attachments`, `results`, and `graphics` buckets. It also holds the
deployment's schema record, every row of the `schema_migrations` and `schema_migration_issues`
Scylla tables except the Job results, as `schema.json` beside the archives (see [The Schema
Record](#the-schema-record)). The Taking Backups guide lists every resource and the component that
backs it up, generated from thoradm's coverage registry, in [What Is Backed
Up](./taking_backups.md#what-is-backed-up). To read the same
list from the installed binary, without a config file or a cluster, run `thoradm backup coverage`;
`--json` prints it as a JSON array with one object per resource, in the same order.

A backup doesn't hold runtime state, which is invalid after a restore, or data a restore rebuilds:

| Not backed up | Why |
|---|---|
| The `logs` and `events` Scylla tables | Stage logs of reactions, which expire by TTL, and a table of trigger events that nothing reads or writes, which only keyspaces older releases created hold |
| The `job_result` rows of the `schema_migrations` Scylla table | The results of upgrade Jobs, which expire after seven days; a restore would make them permanent |
| The Redis keys of reactions, jobs, workers, and services: `reactions:*`, `reactions_group:*`, `reaction_data:*`, `reaction_jobs:*`, `reaction_tags:*`, `reaction_cache:*`, `sub_reactions:*`, `generators:*`, `stage_logs:*`, `logs:*`, `job_queue:*`, `job_data:*`, `workers:*`, `worker_data:*`, `streams:*`, `event-handler:*`, `search-streamer:*`, `cursors:*`, `schema-writer`, `rebucket:*`, `image_ttc:*`, `run_queue:*`, `workload_queue:*`, `workload_ttc:*`, `workloads:*`, and `entities:instance_lock:*` | Runtime state, invalid after a restore |
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
key in the bucket the target's config names for the object's role (files, repos, results,
attachments, graphics), so a target with other bucket names works. Like a backup, it can take hours for a large backup. Before it writes anything it runs a
preflight that verifies the backup and checks the target; [How Restore Uses a
Backup](./taking_backups.md#how-restore-uses-a-backup) in the Taking Backups guide lists every check
and the flag that overrides it.

### Restore Requirements

- **An empty target.** A plain restore refuses a target whose backed-up tables already hold a row,
  and changes nothing. Restore into such a target only with `--merge` or `--wipe <namespace>` (see
  [Merge or Wipe](#merge-or-wipe)). Only the Scylla tables are checked, and not the `nodes` table,
  whose rows a restore keeps (see [What a Restore Keeps of the
  Target](#what-a-restore-keeps-of-the-target)): Redis isn't, since every restore replaces the
  namespace's users, groups, images, and pipelines, and buckets aren't, so a target whose tables are
  empty restores without a flag even when its buckets still hold objects (a restore only overwrites
  an object at the same key).
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
  configure the target's API with the source cluster's `secret_key`, or every restored password
  login fails after the restore (the operator's own users keep the target's passwords, so the
  operator and the components still log in). Thoradm doesn't check it.
- **Redis reachable from thoradm, and no other writer.** Each of the restore's two `schema apply`
  runs (steps 3 and 6 below) holds the namespace's [writer lease](#the-writer-lease) in Redis, so
  the thoradm config must reach Redis even for a backup that holds no `redis.json`. A restore is
  refused while a migration, a schema apply, `reset-state`, or another restore holds the lease
  (`the namespace's writer lease is held by <holder>; wait for it to finish`), and when Redis can't
  be reached (`Redis can't be reached at <host>:<port>, which the restore needs for the namespace's
  writer lease`). The lease is held around each apply and not across the whole restore, because a
  Redis restore can flush the database that holds it, so pause every other writer yourself. A
  restore has no `--force-break-lease`.
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

**Keep the source's `secret_key`.** A backup doesn't record the `secret_key` its password hashes
were made with, and thoradm restores into a deployment with another key without a warning. Before
restoring a backup of another deployment, install the target with the source's key (set
`global.credentials.thoriumSecretKey` to the `thoriumSecretKey` in the source's
`thorium-credentials` Secret; see [Credentials](../deploy/helm-configuration.md#credentials)), or
restore only into a deployment already configured with the same key.
Otherwise every restored password user is locked out until you reset the passwords as [A Restore
into Another `secret_key`](#a-restore-into-another-secret_key) describes. The operator's own users
aren't, since a restore keeps the target's (see [What a Restore Keeps of the
Target](#what-a-restore-keeps-of-the-target)).

What isn't backed up isn't restored (see [What a Backup Holds](#what-a-backup-holds)): the Redis
restore deletes the namespace's reactions, jobs, and worker state, and the excluded Scylla tables
and buckets keep whatever the target held.

### What a Restore Keeps of the Target

A backup of another deployment holds that deployment's operator users and nodes, which the target's
operator and scaler can't use. So a restore, in every mode, keeps the target's own instead:

- **The operator's users** `thorium-operator`, `thorium`, and `thorium-kaboom`, which the Thorium
  operator creates and logs in as with the passwords in its `<user>-pass` Secrets. For each one the
  target holds, the restored user keeps the target's password hash, token, and token expiration,
  and takes the rest of its record (email, role, settings, groups, profile icon) from the backup; a
  user the backup doesn't hold is kept whole, in no group. A backup's operator user the target
  doesn't hold, as when restoring into a namespace whose Redis was emptied, is restored as the
  backup holds it. Every other user, with its password and tokens, comes from the backup. The
  target's users are read through the API's backup of its Redis state before anything is written;
  when the API can't back it up (a namespace that was never initialized, or records that don't fit
  together), no user is kept, and the log and the summary's `warnings` say so.
- **The `nodes` table**, when the target holds a node: the backup's nodes aren't restored, the
  empty-target check doesn't count the table, and `--wipe` doesn't truncate it. A target with no
  nodes gets the backup's, which come back registered and check in again on their own.

The confirmation names both, and the summary's `kept` lists what was kept (`user <name>` for each
user, `table nodes` for the nodes), logged as `restore summary: kept the target's ...`. To restore a
deployment from its own backup, nothing changes: its operator users and nodes are the backup's.

The bootstrap admin (the user in the `bootstrap.admin.secret` Secret, by default `thorium-admin`)
isn't one of the operator's users, so a restore of another deployment's backup replaces it like
every other user: it takes the backup's user of that name, or is gone when the backup has none, and
the restore's confirmation lists it among the replaced users. The operator creates it again only
when its bootstrap runs next, which happens when the bootstrap inputs change (the `ThoriumCluster`'s
`bootstrap` spec, a bootstrap Secret, or the rendered Thorium config). Until then the Secret's
password doesn't log in, and neither does the toolbox import, which logs in as that user. A backup
taken afterwards holds that user as an ordinary admin.

### Merge or Wipe

When the target already holds data, choose what happens to it. `--merge` and `--wipe` exclude each
other.

**`--wipe <namespace>`** rolls the namespace's tables back to the backup. It truncates every
backed-up table the backup holds but `nodes`, whose rows the restore keeps (after dropping its
declared views, which the restore's `schema apply` creates again). It deletes nothing from object storage and sends no S3 request at all, so it never
lists a bucket: every bucket is left as it is. It keeps what the backup doesn't hold: the stage logs
table, the events table where an older release created it, and the buckets. After the restore, the tables and Redis equal the backup and
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
table, `wipe.json` has no `finished` time and lists in `snapshots` the truncate snapshots the
tables already emptied left, and nothing has been restored. A plain re-run is then
refused by the empty-target check, since the tables not yet truncated still hold rows. Fix the cause and run the same `--wipe <namespace>` restore again: it finds the unfinished
`wipe.json` (the newest under `reports/restore-*/`, or in or beside the `--report-dir`), says it is
resuming it, records it as `resumed_from`, wipes what is left, and restores. When the first run left
the target empty, the re-run has nothing to wipe, so it writes no `wipe.json` and records no
`resumed_from`.

**`--merge`** adds the backup's data to a namespace whose own data must stay. Use it only for that:
rows and objects created after the backup survive, rows and objects deleted after the backup come
back, and on a key both hold the backup's values win. Redis is still replaced by the backup's, so
users, groups, images, and pipelines created after the backup are gone, apart from what [What a
Restore Keeps of the Target](#what-a-restore-keeps-of-the-target) lists. To roll a namespace back,
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
the users and groups the Redis restore replaces, and the operator users and nodes it keeps; that the Redis restore first deletes every key of
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
3. **The schema, before loading**, under the writer lease. `schema apply` converges the keyspace to the schema of the
   backup's revision, so the tables exist (a dropped keyspace is created again) and have every
   column the backup's rows carry; nothing is dropped. The files password is checked first (see
   [The Schema Record](#the-schema-record)).
4. **Redis.** The API restores Redis in steps: wipe (every key of the namespace, or with
   `--redis-flush-all` a `FLUSHDB ASYNC` of the database), init, settings, users, scoped tokens,
   groups, images, pipelines, and network-policy sets.
5. **The tables.** Restore drops each table's materialized views just before loading the table.
   Each node is written as registered, with no heartbeat: its workers were Redis state, so each
   node must check in again before the scheduler counts it.
6. **The schema, after loading**, under the writer lease again. `schema apply` runs again and creates every view the table
   restores dropped from the schema, waiting for them to build with no time limit and logging the
   views still building every ten seconds: no API restart is needed. A view whose name a table
   holds is refused, naming the view, and the table has to be dropped by hand (see
   the Views paragraph below). Then restore writes the backup's schema record (see [The Schema
   Record](#the-schema-record)).
7. **The objects**, each uploaded to its original key, addressing buckets the way
   `thorium.s3.use_path_style` says. A failed upload is aborted (no parts are left in the bucket),
   retried if its error is transient, and recorded if it still fails.
8. **The census** of every restored table, and of every table when the backup holds `redis.json`
   (whose restore deletes every census stream), so the listings work at once.
9. **The summary**, logged and written as `summary.json`.

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

**Views.** `schema apply` defines every view, so the restore saves no view statements: it creates
each view it dropped, and any view an interrupted restore left dropped, from the schema, and waits
for them to build, with no time limit, logging every 10 seconds how many views are built and which are
still building (and, on a cluster, how many nodes have finished each). A view that can't be created is
recorded (operation `post-restore`, key `schema apply`) and the summary says the views couldn't be recreated; once the cause is fixed, run
`thoradm schema apply`, which creates them. The schema apply that converges the keyspace before any
row is restored records its failure with operation `pre-restore` and key `schema apply`.

### The Schema Record

A backup holds the deployment's schema record as `schema.json` in the backup directory, beside the
table archives and listed in the manifest's `siblings`: every row of the `schema_migrations` and
`schema_migration_issues` Scylla tables except the Job results, which expire. It is the applied
fingerprint and layout version of each backend, the revision and the full applied history, the
state of each migration pass, the consumed lossy-approval codes, any history an earlier restore
archived, and the data-shaping settings. The file also names the thoradm version that wrote it, and
holds the files password's salted hash, so it is as private as the rest of the backup. Rows of a kind this thoradm doesn't know, such as the audit row a later release writes when it
drops a legacy object, are carried as they are under `other` and written back by a restore, merged
with the target's own rows of those kinds. A deployment
whose keyspace has no `schema_migrations` table has no record, and its backup has no `schema.json`.

A restore with a record:

- **Refuses before writing anything** when the record can't be read or is of a newer format, when
  its revision is one this thoradm has no schema snapshot of (restore with the thoradm version
  named in the file), or, for a backup that holds files (the `s3-ids` or `s3-ids-objects`
  component), when the configured `files.password` isn't the password the backup was taken with.
  Stored files are restored as they were encrypted, so another password would leave files nothing
  opens; a backup of other tables restores whatever the password. The refusal, a `preflight` record for the check `schema`, names the setting
  and never prints a password or a hash.
- **Replaces the target's record** with the backup's after the data is loaded, but only from a
  backup that holds every Scylla table and Redis (`all` or `no-s3`) and whose record names a
  revision. A backup of some components says nothing about the rest, so restoring its record
  would rewind the target's revision under data it never touched: the restore then leaves the
  target's revision, history, and fingerprints alone, writes no `restored` entry, and the
  summary's warnings say why. The record's rows are written over the target's one partition at a
  time, the revision row last, and only then are the rows the backup lacks deleted, so a restore
  that stops partway leaves the old record with some new rows beside it, never none; run the
  restore again to finish it: the applied
  fingerprints and layouts, the revision, the history, the migration passes with their workers and
  issue groups. Restoring a backup taken at an older revision into a deployment at a newer one
  sets the revision to the older one, so the operator's upgrade run finds the steps after it
  unfinished and runs them again once it reads the record (see [Resetting the Recorded
  State](#resetting-the-recorded-state) for when it does). Use the target's thoradm (the API's
  version, see [Restore Requirements](#restore-requirements)): it has a snapshot of every earlier
  revision and converges the keyspace to the backup's revision before loading, so rows with a column
  a later step dropped can be inserted. A backup whose record names a revision newer than the
  target's thoradm knows is refused before anything is written; restore it into a deployment of
  that version or newer.
- **Keeps what it replaces.** Every history entry the restore replaces is kept as an
  `archived_history` row keyed `<the backup's finish time>/<original id>`, so a restore run again
  writes the same keys, and the restored history ends with a
  `restored` entry naming the backup, the backup's revision, the revision it replaced, and
  thoradm's version and the OS user. Consumed lossy-approval codes are merged, never replaced, so a
  code used after the backup was taken still counts as used. A backup of the restored deployment
  carries the archived rows and the `restored` entry.
- **Leaves the data-shaping settings as the restoring cluster has them.** A restore re-buckets
  every row with the cluster's partition sizes, so the settings recorded afterward are the
  cluster's, not the backup's.

A backup without `schema.json` was taken before the record existed (by a pre-Helm or first Helm
release, or by a thoradm without the record) or of a keyspace that never recorded one. Its Scylla
and Redis data is in the layouts of revision `2026-10-v02`, the first Helm revision: the revision
before it, the pre-Helm baseline `2026-10-v01`, differs from it only in Elastic, which a backup
doesn't hold. A restore of such a backup:

- **Records `2026-10-v02` under a target that records a newer revision**, such as a fresh install
  of this release, when the backup holds every Scylla table and Redis (`all` or `no-s3`). The
  target's history is archived as for a backup with a record, the history becomes one `restored`
  entry saying the backup held no record, and the target's fingerprints stay, since the restore
  applied the target's schema. The operator then plans every step after `2026-10-v02` and runs
  them like any upgrade (the data steps wait for their approvals), converting the restored data;
  each step checks every object first and leaves the ones already in the newer layout as they
  are, so data a merge target held is safe. The confirmation names the revision the restore
  records, the summary names the revision recorded, the one it replaced, and the steps the
  operator plans, and the operator starts them once it reads the record (see [Resetting the
  Recorded State](#resetting-the-recorded-state) for when it does).
- **Refuses before writing anything** when this release no longer upgrades from `2026-10-v02`
  (it is older than the release's oldest supported revision, see the upgrade guide's [Upgrade
  paths](../deploy/upgrades.md#upgrade-paths)). The refusal names the release to restore with
  first: restore the backup with that release's thoradm into a cluster of that release, let its
  operator finish the upgrade, then take a new backup and restore it with this release.
- **Leaves the target's record alone** when the target records no revision, or none newer than
  `2026-10-v02`, and when the backup lacks a Scylla table or Redis; the summary's warnings say why
  for a backup that lacks some. The operator treats a deployment with no revision recorded as one
  waiting for `spec.upgrade.starting_revision`.

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
   Thorctl config, so log in (`thorctl login`) as an account the backup holds, or as the operator's
   `thorium` user, which keeps the target's password: the restore replaced the target's other
   users, and the token you restored with may belong to a user the backup doesn't hold.
2. **Rebuild the search indexes.** Elastic isn't restored. Run the search-streamer once with
   `--reindex` to delete the search indexes and rebuild them from the restored data.
3. **Clear Scylla's truncate snapshots** after a `--wipe` restore, once the check passes:
   `nodetool clearsnapshot <namespace>` on every Scylla node frees the disk the wiped data still
   holds (the restore's final report lists the snapshots and prints the command).
4. **Make the operator read the restored record** when the restore moved the revision (the
   summary's `not_done` says so): delete the `thorium-upgrade-state` ConfigMap and restart the
   operator, as [Resetting the Recorded State](#resetting-the-recorded-state) shows. Until it reads
   the record it reports the old revision and doesn't run the steps after the restored one.
5. **Resume the writers and users.** Nodes restored from the backup check in again on their own.

`verify --cluster` writes its report like `verify` and checks:

| Check | Fails when |
|---|---|
| Rows | A backed-up table holds another number of rows in a token range than the backup read from it (the ranges and counts come from the manifest) |
| Objects | An object the backup's indexes list is missing from its bucket, or has another size |
| Redis | A user, scoped token, group, network-policy set, image, or pipeline the API serves differs from `redis.json`, or is in only one of them, or the settings differ; the record names the fields that differ, never their values. Sets (group members, an image's network policies and child filters, a group's network policies, the host path whitelist) compare in any order |
| Kept by design | Never fails: an operator user that differs from `redis.json`, and a token range of `nodes` whose count differs from the backup's, are what a restore keeps of the target (see [What a Restore Keeps of the Target](#what-a-restore-keeps-of-the-target)), so they are logged and counted under the `kept-by-design` check instead of `redis-diff` or the row count |
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

### A Restore into Another `secret_key`

A restore into a deployment whose `thorium.secret_key` differs from the source's succeeds, but no
restored password verifies afterwards (restored tokens keep working until they expire): every
password login of a restored user (`thorctl login`, the Web UI, `POST /api/users/auth`) fails with
`401`. The operator's own users keep the target's passwords (see [What a Restore Keeps of the
Target](#what-a-restore-keeps-of-the-target)), so the operator and the components keep working.

If you have the source's `secret_key`, you can set it as the target's `thoriumSecretKey` and run
`helm upgrade` (see [Changing credentials](../deploy/helm-configuration.md#changing-credentials)).
The restored passwords then verify again, but the operator's own users, whose hashes the restore
kept from the target, no longer do: the operator fails with `401 Unauthorized` until their
passwords are set again under the source's key. Right after the upgrade:

1. **Log in as a restored admin** whose password the source's key verifies, keeping the
   `Authed.token` the response returns.
2. **Set each operator user's password from its Secret** with that admin's token, for
   `thorium-operator`, `thorium`, and `thorium-kaboom`:

   ```bash
   TOKEN=$(printf '%s' '<admin token>' | base64 -w0)
   for user in thorium-operator thorium thorium-kaboom; do
     PASS=$(kubectl -n thorium get secret "$user-pass" -o jsonpath="{.data.$user}" | base64 -d)
     curl -X PATCH "<api>/api/users/user/$user" -H "Authorization: token $TOKEN" \
       -H "content-type: application/json" -d "{\"password\": \"$PASS\"}"
   done
   ```

3. **Restart the operator** (`kubectl -n thorium rollout restart deployment operator`), which then
   logs in again and returns to `Ready`.

A password you reset under the target's key before the upgrade stops verifying too. Without the
source's `secret_key`, reset the passwords under the target's key instead:

1. **Log in as an admin**: the operator's `thorium` user, whose password is in the `thorium-pass`
   Secret (`kubectl -n thorium get secret thorium-pass -o jsonpath='{.data.thorium}' | base64 -d`),
   an admin whose restored token still works, or an admin you bootstrap with the target's
   `secret_key` under a username the backup doesn't hold, keeping the `Authed.token` the response
   returns:

   ```bash
   curl -X POST <api>/api/users/ -H "secret-key: <secret_key>" -H "content-type: application/json" \
     -d '{"username": "<name>", "password": "<password>", "email": "<email>", "role": "Admin",
          "local": true, "skip_verification": true}'
   ```

2. **Reset the restored password users** with that admin's token (`PATCH /api/users/user/<user>`
   with `{"password": ...}`) and send each user their new password. LDAP users log in as before:

   ```bash
   TOKEN=$(printf '%s' '<admin token>' | base64 -w0)
   curl -X PATCH "<api>/api/users/user/<user>" -H "Authorization: token $TOKEN" \
     -H "content-type: application/json" -d '{"password": "<new password>"}'
   ```

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
| `backup restore` | `reports/restore-<time>/` in the backup, or `--report-dir`: `failures.jsonl`, `verify.json` (the preflight's verify), `wipe.json` (with `--wipe`), and `summary.json` |
| `backup verify` | `reports/verify-<time>/` in the backup, or `--report-dir`: `failures.jsonl` and `verify.json` |
| `backup scrub` | `reports/scrub-<time>/failures.jsonl` in the backup |

A restore's `summary.json` holds what each component restored, skipped, and failed, the failures,
whether the backup was restored as legacy, what `--merge` merged into or `--wipe` truncated, the
objects the backup is missing, the views recreated, the schema record restored, the census rebuilt, and its warnings; it is
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
steps between them, and runs, checks, reverts, and records the data migrations behind them.
`list` and `plan` read no config, so they run anywhere, including inside the cluster with
`kubectl -n thorium exec deploy/api -- /app/thoradm migrate list`. Every other subcommand reads the
cluster and needs the thoradm config. The operator runs these commands itself as Jobs during an
upgrade; run one by hand to preview a migration, repair issues, or recover.

```Bash
thoradm migrate list
thoradm migrate plan --from <revision> [--to <revision>]
thoradm migrate check --id <migration> [--all] [--samples] [--slices <n>] [--batch-rows <n>]
thoradm migrate run --id <migration> [--objects <keys>] [--reason <code>] [--accept-issues <code>]
                    [--confirm <code>] [--backup <ref>] [--slices <n>] [--batch-rows <n>]
                    [--force-break-lease]
thoradm migrate verify --id <migration>
thoradm migrate issues --id <migration> [--reason <code>]
thoradm migrate report --id <migration>
thoradm migrate revert --to <revision> [--dry-run] [--backup <ref>] [--confirm <code>]
                       [--accept-backup-gaps]
thoradm migrate reset-state --to <revision> --i-restored-data
thoradm migrate record --revision <revision> [--state <file>] [--seed]
```

`list` prints every revision edge and its steps. A release edge marked `one-way` has no reverse
edge, so a cluster can't be downgraded across it; `revert` marks an edge that goes back to an older
revision. A feature branch between releases can also add `pre-release` edges (to or from its
`-pre.<change>` revision) and `sibling` edges (an alternative path to a depth the main chain already
reaches, which `auto_target_dev` never targets); the shipped catalog has none. A step shows its kind, `needs approval + backup` when it
changes data, the component it `scales down`, and the `migration` it runs:

```
2026-10-v01 (released with Thorium 1.8.1): Thorium deployed by the minithor or megathor scripts before the Helm charts (the baseline every converted cluster starts from)
2026-10-v02 (released with Thorium 1.8.1; from 2026-10-v01; one-way): Thorium deployed by the Helm charts with the chart-managed operator
  - scylla-role [Infra]: Check that Thorium's Scylla role logs in with the configured credentials
  - elastic-identity [Infra]: Check that Thorium's Elastic user authenticates and holds the index privileges Thorium needs
  - elastic-reindex-keyword-mappings [Data, needs approval + backup, scales down search-streamer, migration elastic-rebuild]: Reindex Elastic indexes created without mappings, whose group field is not a keyword (megathor deployments before the indexes were left to the search-streamer); skipped when every index is already correct
  - finalize [Config]: Record the new revision
2026-10-v03 (released with Thorium 1.8.1; from 2026-10-v02): Schema apply runs before any step that changes data
  - schema-apply [Infra]: Apply the schema in a Job before any step that changes data, creating missing tables, columns, and views and recording what was applied
  - users-settings-theme [Data, needs approval + backup, scales down api, migration users-settings-theme]: Move each user's theme from its own field into the user's settings
2026-10-v02 (released with Thorium 1.8.1; from 2026-10-v03; revert): Revert the revision that applies the schema first
  - users-settings-theme-reverse [Data, needs approval + backup, scales down api, migration users-settings-theme-reverse]: Move the theme of any user stored in its own field into the user's settings, the layout the first Helm revision reads; users already there are skipped
```

`plan` prints the steps from one revision to another (the latest by default), for example
`thoradm migrate plan --from 2026-10-v01`. Find a cluster's revision in the `Revision` column of
`kubectl -n thorium get thoriumcluster`, or with `thoradm schema status`. A target the catalog has
no path to exits 1.

**The migrations.** `--id` names a migration, which a step's `migration` in `list` names. The
migrations built into this release are `elastic-rebuild`; the users migrations, which move a user's
theme between its own field and the user's `settings` (`users-settings-theme` on the way up to
`2026-10-v03`, `users-settings-theme-reverse` on the way back from it to `2026-10-v02`, which skips
every user already in the layout `2026-10-v02` reads); and the six
`rebucket-<data type>` migrations (`rebucket-files`, `rebucket-repos`, `rebucket-results`,
`rebucket-entities`, `rebucket-associations`, `rebucket-tags`) that the operator plans when a
partition size changes (see [Changing a partition size](../deploy/upgrades.md#changing-a-partition-size)).
Thoradm can check, run, and verify every one of them, `elastic-rebuild` included, which the operator
runs as the migration Job of the Elasticsearch reindex step (see [Reindexing
Elasticsearch](../deploy/upgrades.md#reindexing-elasticsearch)). An unknown id exits 1. A
migration thoradm can't perform prints its manual procedure and exits 3 on `check`, `run`, and
`verify`, and one it can only check does so on `run` and `verify`; this release registers neither
kind.

| Subcommand | What it does |
|---|---|
| `check` | Classifies every object the migration would touch (`current`, `migratable`, `unrecognized`, `error`) and writes nothing but its own result. It prints the report as JSON: the counts, the rows a run would rewrite, how long the paused components would stay down, the largest issue groups by reason code, and, for a step that loses data, the loss estimate. `--all` prints every group and `--samples` up to 20 sample keys each, to the terminal only. Exits 0 |
| `run` | Runs the migration: it splits the work into `--slices` (default 16) contiguous slices, one worker each, and checkpoints after every batch of at most `--batch-rows` rows (default 10000), so a stopped run resumes where it stopped. It prints a JSON summary with the status, the counts, and the issue groups. A pass that finishes, with groups within the limit or accepted, exits 0 |
| `run --objects <keys>` | Processes only these object keys, such as objects you fixed by hand after an issue |
| `run --reason <code>` | Re-runs one issue group: scans the family, writes the objects that now convert, and recounts every group. It prints `group <code>: <n> objects before, <m> after` and exits 5 while the group still holds objects. When it ends the pass, the operator continues without a new approval |
| `run --accept-issues <code>` | Reason codes whose groups don't fail the run (the operator passes the approval's `acceptIssues`; a code that matches no group is ignored) |
| `run --confirm <code>` | The confirmation code of a lossy step. Thoradm records the code as consumed before anything else, and refuses a code already consumed (see [Accepting loss](../deploy/upgrades.md#accepting-loss)) |
| `run --force-break-lease` | Takes the writer lease although another writer holds it (see [The Writer Lease](#the-writer-lease)) |
| `run --backup <ref>`, `--running-entry <json>` | Passed by the operator: the approval's backup reference, recorded in the pass, and the step's `Running` history entry with its paused replica counts, written before anything else |
| `verify` | Loads every object of the migration's family through the API's own deserialization. Exits 0 when all load, and 5 when some don't |
| `issues` | Lists the issue groups of the current attempt, summed across workers. `--reason <code>` shows one group: its class, count, resolved count, whether it was accepted, and its sample keys from every worker |
| `report` | Prints what the migration's last pass applied, as JSON; exits 1 when the pass hasn't ended |
| `revert` | Moves a cluster back to an older revision: it reads the recorded revision and history, plans the reverse path, and runs each step's migration in turn. `--dry-run` prints the steps, what each loses, and the confirmation code of each lossy one, and changes nothing. `--dry-run` also shows whether `thoradm backup` covers each lost field, and names the components a step expects to be stopped while it runs; the revert stops none of them, so scale them to zero first. A step that loses data runs only with `--confirm <its code>` (and `--backup`), one lossy step per run: the next code exists once this step's entries are recorded. A step that loses a field no `thoradm backup` covers, which a backup can't restore, is refused unless `--accept-backup-gaps` is also given, the way the operator's approval needs `acceptBackupGaps`. A step thoradm doesn't run stops the revert with that step's manual procedure. An interrupted step exits 4; run the revert again to resume it. An interrupted lossy step's code changed when its run was recorded, so run `--dry-run` again for its new code and rerun with that code. When every step has run it records the older revision |
| `reset-state` | Rewrites the recorded revision after data was restored or wiped other than by `backup restore`, when the record names a revision the data isn't at; refuses with exit 2 without `--i-restored-data` (see below) |
| `record` | Run by the operator to copy its entries into the record and move the recorded revision once every step is recorded; `--seed` records the revision and history of the `--state` ConfigMap when the keyspace holds none |

Exit codes of the `migrate` and `schema` subcommands:

| Exit code | Meaning |
|---|---|
| 0 | Done: the command ran, and a migration pass ended within its limits |
| 1 | A refusal or an error, such as an unknown id, no recorded revision, or a held lease (see stderr) |
| 2 | The config or an argument can't be used |
| 3 | A migration thoradm can't run, a revision with no committed snapshot to converge to, or a backend a read-only command (`schema diff`, `schema legacy`) can't reach |
| 4 | A failure a retry may get past: a lost connection, an interrupted run (SIGTERM, resumable), or an unreachable Redis |
| 5 | A migration ended `Failed` because an issue group went over the step's limit, a `verify` found objects that don't load, or a `--reason` group still holds objects |
| 6 | A migration failed systematically: the migration, not the data, is wrong |
| 7 | `schema diff` found drift |

Exit codes 1 to 3, 5, and 6 fail a Job at once, and 4 is retried, which is how the operator treats a
migration Job.

### Issue groups and reason codes

A migration classifies each object it reads. An object that can't be converted is not written and
is recorded in an issue group named by a reason code (for example `malformed_json:settings`), with
up to 20 sample keys per worker. A group that holds more objects than the step's limit fails the
run; a smaller one is recorded and the run finishes with issues, which the `MigrationIssues`
condition reports. Fix the objects and re-run the group with `run --reason <code>`, or accept the
code with `acceptIssues` (see [Data migrations](../deploy/upgrades.md#data-migrations)).

### The Writer Lease

Commands that write take the namespace's writer lease, so at most one writer runs at a time: the
operator's Jobs, `thoradm migrate run`, `thoradm schema apply` (not `--dry-run`), `thoradm migrate reset-state`, the two schema applies of a
restore, and `thoradm migrate revert` through the runs it makes. The lease is the Redis key
`<namespace>:schema-writer`, renewed by a heartbeat every ten seconds while it is held and expiring
after 30 seconds without one. A writer whose lease is broken or lost stops at its next batch. A
command that finds the lease held exits 1 with `the namespace's writer lease is held by <holder>;
wait for it to finish`, and Redis failing exits 4. `--force-break-lease`, on `migrate run` and
`schema apply` only, takes the lease over even though another writer holds it, and the holder's
workers stop at their next batch; use it only when the holder is gone, such as a crashed Job whose
lease hasn't expired.

### Resetting the Recorded State

`thoradm migrate reset-state --to <revision> --i-restored-data` fixes a record that names a
revision the data isn't at, as after restoring a backup taken at an older revision or wiping data
with tools other than `thoradm backup restore` (which writes the backup's record itself, or for a
backup without one the revision its data is at). Without
`--i-restored-data` it refuses with exit code 2. It takes the writer lease, so it refuses while a Job or another
command is writing, voids the migration work and recorded pause counts of revisions above the
target, appends one `reset-state` entry to the history, and writes the revision row last, so a
stopped run leaves the old revision and can be run again. The entry lowers the revision, so every
step above the target counts as unfinished again although its old `Done` entry stays in the
history, and the operator's upgrade run runs those steps again once a target is set. The
operator reads the record when its hourly drift read is due, at its next reconcile after that
(at least daily), and within seconds after that read, and rebuilds its ConfigMap mirror from it
when it holds a `reset-state` or `restored` entry the mirror lacks. Until then it plans from its
`thorium-upgrade-state` ConfigMap, and an operator older than the revision that ConfigMap holds
refuses the cluster (`AheadOfCatalog`) without ever reading the record. After a restore or a
reset, delete the ConfigMap and restart the operator to make it read the record at once (a quiet
cluster may not reconcile again for a day, so deleting the ConfigMap alone isn't enough):

```bash
kubectl -n thorium delete configmap thorium-upgrade-state
kubectl -n thorium rollout restart deployment operator
```

## The Declared Schema

`thoradm schema` works with the schema this build declares: the Scylla tables and materialized
views, Elasticsearch's indexes, and the layout versions of Redis and S3.

```Bash
thoradm schema print [--backend scylla|elastic|redis|s3] [--canonical]
thoradm schema hash [--backend scylla|elastic|redis|s3]
thoradm schema apply [--dry-run] [--wait-for-scylla <secs>] [--seed-revision <revision>]
                     [--to-revision <revision>] [--accept-partition-sizes]
                     [--recreate-view <view>]... [--yes] [--force-break-lease]
thoradm schema status [--acknowledged <json>] [--no-probe] [--result <job uid>]
                      [--wait-for-scylla <secs>]
thoradm schema diff [--json] [--acknowledged <file>]
thoradm schema verify-views [--view <view>] [--sample <n>]
thoradm schema legacy [--json]
thoradm schema drop-legacy --object <backend:name> [--execute] [--confirm]
```

| Subcommand | What it does |
|---|---|
| `print` | Prints the declared schema as CQL, or with `--canonical` in canonical JSON, optionally for one backend. Reads no config |
| `hash` | Prints the declared fingerprints: the deployment's and each backend's. Reads no config |
| `apply` | Creates the keyspace and the `schema_migrations` tables, then converges every declared object: it creates what is missing and adds missing columns, and it never drops anything. It records what it applied. `--dry-run` prints the statements and takes no lease. The operator runs the same command as the schema-apply Job before it rolls components out |
| `status` | Prints the deployment's record (revision, fingerprints, history), a capped drift summary, and this thoradm's catalog identity on one `THORIUM-RESULT` line, read-only. `--no-probe` skips the probe of the live schema and reports the record only |
| `diff` | Compares the live schema with the snapshot of the recorded revision (see [Comparing the Live Schema](#comparing-the-live-schema)) |
| `verify-views` | Compares each declared materialized view with its base table by token range, read-only; `--sample <n>` checks only that many random token ranges. Scylla updates views in the background, so rows written while it runs can show a view briefly out of step: run it again, with writers paused, before recreating a view it reports |
| `legacy`, `drop-legacy` | List and drop the objects older releases created (see [Legacy Schema Objects](#legacy-schema-objects)) |

`schema apply` refuses, and exits 1, when a data-shaping setting (`thorium.files.partition_size`
and the other partition sizes, or `files.password`) differs from the recorded one while data shaped
by it exists, naming the setting and never printing a value. The operator then plans the matching
`rebucket-<data type>` step (see [Changing a partition size](../deploy/upgrades.md#changing-a-partition-size)).
`--accept-partition-sizes` records the configured partition sizes although sampled rows suggest
others, and the operator passes it once the schema-apply step is approved. A table whose configured
default TTL differs from the live one is altered.

- `--seed-revision <revision>` records that revision when `schema_migrations` holds none, after
  probing the live schema against the revision's snapshot; it is refused when another revision is
  recorded or the live schema doesn't match.
- `--to-revision <revision>` (which excludes `--seed-revision` and `--recreate-view`) converges to
  an older revision's committed snapshot instead of the declared schema: it creates the tables, columns, and views that snapshot has and the live schema
  lacks (what a later step dropped) and drops nothing a newer revision added. It is the first step
  of reversing a step that dropped something.
- `--recreate-view <view>` (repeatable) drops a declared materialized view and recreates it from the
  declaration, then waits for it to build. Routes that read the view fail while it is dropped and
  then see missing rows until its build finishes. It asks for confirmation unless `--yes` is given,
  which is required without a terminal. A create that fails after the drop leaves the view missing;
  run the command again.
- When a table holds the name of a declared materialized view, `schema apply` and
  `--recreate-view` refuse before changing anything, naming the view. Neither drops a table, so
  drop it by hand once you have checked it holds nothing you need; the next `schema apply` creates
  the view. `schema diff` reports the table as drift either way.
- When a materialized view holds the name of a declared table, `schema apply` refuses the same
  way before changing anything, naming the table. It never drops a view, so drop it by hand once
  you have checked nothing needs it; the next `schema apply` creates the table.
- `--wait-for-scylla <secs>` waits up to that long for Scylla to accept connections.

`schema apply` exits 0 on success, 1 when it refuses, 2 for an unusable config or flag, 3 when
`--to-revision` names a revision with no committed snapshot, and 4 when Scylla or Redis can't be
reached or a statement failed in a way a retry may get past.

## Comparing the Live Schema

`thoradm schema diff` probes Scylla, Redis, S3, and Elasticsearch read-only and compares what it finds
with the schema snapshot of the revision the deployment's record names. It prints every difference
(`--json` prints the report as JSON), marking an object `missing`, `changed`, or `unexpected`.
The snapshot is the recorded revision's, or, once a schema apply has created the objects of a later
revision before the record moves to it, that later revision's, so an upgrade in progress doesn't
show its own new objects as `unexpected`. `--acknowledged <file>` takes a JSON list in the form of the `ThoriumCluster`'s
`spec.upgrade.drift_acknowledged`; an acknowledged `unexpected` object is not drift.

| Exit code | Meaning |
|---|---|
| 0 | Every backend matches the recorded revision (or only acknowledged objects differ) |
| 1 | A refusal, such as no recorded revision to compare with |
| 2 | The config or the acknowledgements file can't be used |
| 3 | A backend the comparison needs is unavailable |
| 4 | A read failed in a way a retry may get past |
| 7 | The report was printed and at least one object differs |

## Legacy Schema Objects

Older releases created two Scylla objects that Thorium no longer uses: the `events` table and the
`entities_by_name` view. Thorium no longer creates them, a keyspace an older release created keeps
both with their rows, and starting Thorium against that keyspace leaves them alone. They cost disk
and, for the view, write work, and an admin can remove them on purpose. Two commands list and drop
them. Neither runs unless you type it, and `thoradm schema apply` never drops anything.

Each object is named `<backend>:<name>`, here `scylla:events` and `scylla:entities_by_name`. A
legacy object is one Thorium's registry of retired objects lists; only those can be dropped.

### Listing Legacy Objects

`thoradm schema legacy` only reads. It prints two sections and a count, or one JSON document with
`--json`.

```Bash
thoradm schema legacy [--json]
```

```
Legacy objects
  scylla:events  (table, class Runtime)
    present:     yes
    rows:        18204
    created by:  2026-10-v01, 2026-10-v02
    used by:     none
    downgrade to 2026-10-v03: recreated (it does not use this object)
    downgrade to 2026-10-v02: recreated (it does not use this object)
    downgrade to 2026-10-v01: recreated (it does not use this object)
    droppable:   yes
  scylla:entities_by_name  (view, class Derived)
    ...

Not registered (cannot be dropped by this command)
  none

no unmanaged extra objects
```

The first section has one block per registered object: whether the keyspace holds it, its row count,
the revisions that created it and the revisions that read it, what a downgrade to each revision you
can still move to does to it, and whether it can be dropped. A downgrade line says one of:

| Line | Meaning |
|---|---|
| `recreated (it does not use this object)` | The revision doesn't use the object, so a downgrade creates it again |
| `recreated and rebuilt from the base table` | The revision uses the object and its data can be recreated |
| `DATA LOSS (N rows)` | The revision uses the object and its rows are the only copy, so dropping it would lose them |

The second section lists each object that is live and named in a past revision's schema but that
Thorium neither declares nor registers as legacy. Thoradm doesn't know whether any revision needs
such an object, so no command drops it; each entry names the revisions that list it and says to ask
a developer to register it as legacy or declare it again. The last line counts the live objects no
revision names, which no Thorium command touches; `thoradm schema diff` lists them.

### Dropping Legacy Objects

```Bash
thoradm schema drop-legacy --object <id> [--object <id> ...] [--execute] [--confirm]
```

Every object is named with `--object`; there is no way to drop all of them at once. Without
`--execute` the command is a dry run: it prints each object's row count, the statement it would
run, and what a downgrade does to the object afterwards, and drops nothing.

```Bash
# see what dropping the events table would do
thoradm schema drop-legacy --object scylla:events
# drop both legacy objects, typing each id when asked
thoradm schema drop-legacy --object scylla:events --object scylla:entities_by_name --execute
```

With `--execute` the command asks you to type each object's id before it drops anything; a wrong
answer drops nothing. `--confirm` skips the questions, and is required with `--execute` when no terminal is attached.
An object the keyspace doesn't hold is skipped.

The command refuses, exits 1, and drops nothing in the whole list when any object is:

```
scylla:samples_list is a declared object; refusing.
scylla:tools is not a registered legacy object. Registered: scylla:events, scylla:entities_by_name.
refused: scylla:foo is used by 2026-10-v02 (required data, 42 rows); a downgrade would lose it.
```

That is, an object Thorium declares, an id that isn't a registered legacy object (including an
object only a past revision names), or an object that a revision you can still move to uses with
rows that are the only copy. It also refuses when the keyspace has no `schema_migrations` table,
since a drop must be recorded: run `thoradm schema apply` first.

Each drop writes one row to `schema_migrations` under the kind `legacy_drop`, holding the time,
the object's id, how many rows it held, and the operating system user and host that ran the command.
A dry run writes nothing. The rows are part of a backup's schema record (`schema.json`), like the rest
of `schema_migrations` except the Job results, and a restore writes them back.

| Exit code | Meaning |
|---|---|
| 0 | Done, or a dry run |
| 1 | A refusal, or a drop that was refused |
| 2 | The config can't be loaded |
| 3 | Scylla is unavailable |
| 4 | A read or statement failed in a way a retry may get past |

A dropped legacy object comes back when a deployment downgrades to a revision whose schema
lists it, because a downgrade creates every object the target revision lists and the keyspace lacks.
Dropping them first doesn't stop `2026-10-v01` from moving to `2026-10-v02` and later.
