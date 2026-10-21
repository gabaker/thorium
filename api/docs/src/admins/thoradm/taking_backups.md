# Taking Backups

This guide covers taking a backup of a Thorium cluster with `thoradm backup new`: what you need
before you start, how to choose what is backed up, where the backup goes, how to read the results,
and every file and directory a backup writes. Restoring is covered briefly at the end and in the
[Thoradm guide](./thoradm.md#restoring-a-backup).

## Before You Start

### Where to run thoradm

Thoradm reads Thorium's data directly from its backing services and writes the backup to a local
directory, so run it on a host that can reach:

- every Scylla node named in the cluster config,
- the S3 endpoint named in the cluster config,
- the Thorium API named in your Thorctl config (the Redis data is fetched through the API),

and that has enough disk for the backup (see [Disk space and permissions](#disk-space-and-permissions)).
A large backup takes hours, so run it in the background or in a detached `tmux` session.

### The configs thoradm needs

Thoradm reads two configs:

- **The cluster config** (`--cluster-conf`/`-c`, default `./thorium.yml`): Scylla nodes and
  credentials, the S3 endpoint and credentials, the bucket names, and the Thorium namespace. See
  [Config](./thoradm.md#config) for how to build it from the `ThoriumCluster` resource.
- **The Thorctl config** (`--ctl-conf`, default `~/.thorium/config.yml`): the API URL and a token.
  Log in as a Thorium **admin** with `thorctl login` first; the Redis data (users, groups, images,
  pipelines, and system settings) is only served to admins.

### Run thoradm at the API's version

Thoradm and the API each read the Redis data into their own copy of it, so when they come from
different Thorium versions, records or fields one version doesn't know are dropped without a word.
Use the thoradm of the same Thorium version as the API: the version must match exactly, patch
included (`1.8.1` and `1.8.2` don't match). Thoradm reads the API's version from the
`thorium-version` header of its answers, and refuses a version other than its own, or an answer
without the header, before it writes anything:

- `backup new` checks when it backs up the `redis` component (the default list, `all`, and `no-s3`
  include it); a backup without it doesn't ask.
- `backup restore` checks when the backup holds `redis.json`, or with `--wipe <namespace>`; the
  refusal is a `version` record in the restore's `failures.jsonl`.
- `backup verify --cluster` only logs a mismatch at WARN and goes on, so differences it finds in the
  Redis data may come from the versions.
- The offline `backup verify` and `backup scrub` don't talk to the API and don't check.

`--allow-version-mismatch` on `backup new` or `backup restore` lets the run go ahead anyway: thoradm
logs the mismatch at WARN, and records both versions in the backup's manifest
(`api_version_mismatch`) or the restore's summary (`warnings`). Use it only when you can't run a
matching thoradm and accept that the Redis data may lose what the other version lacks.

The check compares version numbers only, so it tells thoradm and the API apart only when they were
released with different versions: a thoradm and an API built from different code that report the
same version pass it. An API older than this thoradm's release clears Redis for a restore with
`FLUSHDB`, which deletes every key in the API's Redis database, other namespaces' included, rather
than only the restored namespace's keys; a restore through such an API does that whenever the check
lets it through, because the versions match or `--allow-version-mismatch` was given.

### Quiesce writers for a consistent backup

A backup is taken table by table while the cluster runs; it is not a snapshot. Rows written while
the backup runs may or may not be in it, and a sample uploaded after its table was read but before
its object was copied is in neither. For a consistent backup, stop the writers first: tell users,
and scale the scaler, event handler, search streamer, and reactors to zero until the backup ends.

### Disk space and permissions

A backup holds a copy of every backed-up table, every object those tables name in the `files`,
`repos`, `attachments`, and `results` buckets, and every graphic those tables and the users name
in the `graphics` bucket, uncompressed, so it needs about as much space as those buckets plus the
tables. Table data is
written in archive files of up to 10 GiB each.

A backup holds every password hash and API token in Thorium, so anyone who can read it holds admin
credentials. Thoradm keeps it private to the user running it: the backup directory and every
directory in it are mode `0700`, and every file is mode `0600`. A directory that already exists and
that other users can read or enter is changed to `0700` before anything is written; if its mode
can't be changed (it belongs to another user, for example), the backup is refused. Keep backups on
private, access-controlled storage. Backups are not encrypted.

## Taking a Backup

```bash
thoradm backup new --output /mnt/backups/thorium-2026-10-17
```

```text
thoradm backup new [COMPONENTS] [OPTIONS]
```

### Choosing components

The positional argument is a comma-separated list of components; it defaults to `all`.

| Component | What it backs up |
|---|---|
| `all` | Every component below |
| `no-s3` | Every component except the four S3 components (`s3-ids-objects`, `comment-attachments`, `result-files`, `graphics`); an S3 component listed beside it is still backed up |
| `redis` | Users and their scoped tokens, groups and the network policies each holds, images, pipelines, and system settings, fetched through the API into `redis.json` |
| `samples-list` | The `samples_list` table |
| `s3-ids` | The `s3_ids` table, which names the objects of samples and repos |
| `s3-ids-objects` | The objects of samples and repos in the `files` and `repos` buckets |
| `comments` | The `comments` table |
| `comment-attachments` | The comment attachments in the `attachments` bucket |
| `results` | The `results` table |
| `result-files` | The result files in the `results` bucket, and the entity blob of each kind of entity a result found (`<result id>/__thorium_entities/<kind>.json`) |
| `results-stream` | The `results_stream` table |
| `tags` | The `tags` table |
| `repo-data` | The `repo_data` table |
| `repo-list` | The `repos_list` table |
| `commitish` | The `commitishes` table |
| `commitish-list` | The `commitish_list` table |
| `network-policies` | The `network_policies` table |
| `entities` | The `entities` table |
| `associations` | The `associations` table |
| `notifications` | The `notifications` table |
| `nodes` | The `nodes` table |
| `graphics` | The user icons and entity images the backup's own records name: the `image` of each row of the `entities` table and of each user in the Redis state. Needs `entities` and `redis` in the same run |

If both `all` and `no-s3` are given, `all` wins.

Components run in a fixed order: every table first, then `redis`, then the S3 components. An S3
component finds the objects to copy in the archives its table wrote in the **same run**:
`s3-ids-objects` reads the `s3-ids` archives, `comment-attachments` the `comments` archives, and
`result-files` the `results` archives (each result's files and entity blobs). An S3 component
listed without its table is not run; it is recorded in `failures.jsonl` and the backup exits 2.
Back up each S3 component with its table, for example
`thoradm backup new results,result-files -o <dir>`.

**Graphics are backed up by what the records name.** `graphics` never lists the `graphics`
bucket. It reads the `image` column of the `entities` archives and the `image` of each user in
`redis.json`, both written in the same run, and copies the object each one names. So `graphics`
needs the `entities` and `redis` components: `thoradm backup new graphics -o <dir>` is refused as a
usage error (exit 1, nothing written) with the message `graphics needs entities and redis`, naming
whichever of the two the selection lacks, and `graphics,entities,redis` and `all` (what a plain
`backup new` runs) are accepted. A resumed backup is held to the same rule, with the components
the interrupted run recorded counting. A graphic a record names that the bucket lacks is a failed
object in `failures.jsonl` and the backup exits 2.

An **orphan** graphic, an object in the bucket that no entity row and no user names (for example
one left behind by a failed delete), is not backed up and so is not restored. Because the keys come
from this namespace's own records, a graphics bucket shared between namespaces no longer puts the
other namespaces' icons and images in the backup. A backup taken before this rule, with a
`graphics/` directory that holds every object of the bucket, still verifies and restores as it
always did.

### Flags

| Flag | Default | What it does |
|---|---|---|
| `--output`, `-o` | `ThoriumBackups` | The backup directory. It must be new, empty, or an interrupted backup to resume (see [The output directory](#the-output-directory)) |
| `--workers`, `-w` | `10` | A global flag, given before `backup`: the workers per component (at least 1) |
| `--multiplier`, `-m` | `100` | Each table's token ring is split into workers × multiplier ranges (at least 1) |
| `--retries` | `5` | Attempts per object, including the first, before its failure is recorded (at least 1). A transient error (a timeout, a reset connection, a busy node) is retried after a growing delay |
| `--s3-concurrency` | `64` | The most S3 downloads in flight at once, across every S3 component of the run (1 to 65,536) |
| `--abort-on-failure` | off | Stop at the first object that fails: record it, exit 1, and leave the backup to be resumed. Without it the backup carries on past every failure |
| `--allow-version-mismatch` | off | Back up the `redis` component even when the API reports a Thorium version other than thoradm's, or none; the mismatch is logged at WARN and recorded in the manifest. Without it such a backup is refused (exit 1) before anything is written (see [Run thoradm at the API's version](#run-thoradm-at-the-apis-version)) |
| `--prune-dangling` | off | Drop the user and group references of the `redis` component that stay dangling, instead of failing the component; each change is listed in `pruned.jsonl`, the run exits 2, and a restore of the backup needs `--accept-failures` (see [Pruning dangling references](#pruning-dangling-references)) |
| `--accept-broken-links` | off | Go on past a broken link, a row that names an S3 object the cluster lacks, instead of failing the object: the link is kept as it is, recorded in `failures.jsonl` as a `broken-link` record, and counted apart in the manifest. The run exits 2, and a restore of a backup whose only records are accepted broken links needs no `--accept-failures` (see [Accepting broken links](#accepting-broken-links)) |
| `--cluster-conf`, `-c` | `./thorium.yml` | A global flag: the cluster config |
| `--ctl-conf` | `~/.thorium/config.yml` | A global flag: the Thorctl config |

```bash
thoradm --workers 20 backup new no-s3,s3-ids-objects -m 50 --retries 8 -o /mnt/backups/thorium-2026-10-17
```

### The output directory

Each backup goes in its own directory. What `--output` holds decides what the run does:

| The directory | What `backup new` does |
|---|---|
| Doesn't exist, or holds no files (empty directories at most) | Starts a new backup, creating the directory (and its parents) with mode `0700` |
| Holds a finished backup (`manifest.json`) | Refuses: a finished backup is never changed. Back up into a new directory |
| Holds an interrupted backup (`backup-in-progress.json` and no `manifest.json`) | Resumes it (see [Interrupted backups](#interrupted-backups)) |
| Holds other files (neither file above) | Refuses: the files belong to no backup this run can resume. Use a new or empty directory |
| Another thoradm run is writing to it | Refuses and exits 1, naming the other run's `backup.lock` |

Because a finished directory is refused, a scheduled backup needs a new directory per run, for
example one named by date:

```bash
thoradm backup new -o /mnt/backups/thorium-$(date +%Y-%m-%dT%H%M)
```

Running `results` and then `result-files` into the same directory as two separate runs doesn't work:
the second run is refused. Request both in one run. Adding components to a finished backup is
planned as a future feature.

**`backup.lock`.** A run holds an exclusive lock (`flock`) on `backup.lock` in the directory until
it ends, however it ends, so two runs never write the same backup. A second run into a directory in
use exits 1 at once. The file stays after the run; a lock file left by a run that has ended doesn't
stop the next run.

### Interrupted backups

A run that is killed, crashes, or is stopped by `--abort-on-failure` leaves `backup-in-progress.json`
and no `manifest.json`. Restore refuses such a directory. To finish it, run `thoradm backup new` into
the same directory again:

- the run backs up every component the interrupted run was asked for, plus any you add;
- every table is read again from the start, replacing its archives;
- `redis.json` is fetched again;
- an object already in the backup is kept when its file's size matches the object in S3 and the
  interrupted run's object index, and its sha256 matches the index; otherwise it is downloaded again.
  Files of objects no table row names any more are removed;
- the interrupted run's `failures.jsonl`, if it recorded anything, moves to
  `reports/backup-<start time>/failures.jsonl`, so the new `failures.jsonl` and the manifest count
  only what the finished backup is missing.

### Pruning dangling references

The `redis` component fails, with one `failures.jsonl` record per problem and no `redis.json`, when
the Redis records don't fit together, as a restore would refuse them. Among the users and groups
that means:

- a group whose owners, managers, users, or monitors (direct or combined) name a user the backup
  doesn't hold;
- a user whose groups name a group the backup doesn't hold.

The API reads the records up to three times (200 ms, then 400 ms apart) before it fails, so a change
made while it reads (a user deleted mid-backup) doesn't fail the backup. A reference that stays
dangling, left behind by an earlier failure, fails every backup until it is repaired. To take a
backup anyway, pass `--prune-dangling`:

```bash
thoradm backup new --prune-dangling -o /mnt/backups/thorium-2026-10-17
```

A reference is dropped only when it is dangling on all three reads and the API then confirms, by
looking the user or group up in Redis, that it doesn't exist. One that resolves on a later read, or
whose user or group exists by the time it is looked up, is kept, and a kept reference that is still
dangling fails the backup as without the flag. With the flag, the backup drops:

| Reference | What is dropped |
|---|---|
| A group's owners, managers, users, or monitors naming a user that is gone | The name, from the role's direct and combined members |
| A user's groups naming a group that is gone | The group, from the user's groups |
| A scoped token's groups naming a group that is gone | The group, from the token's groups, which narrows the token; a token left with no group is deleted whole, since Thorium refuses a scoped token without a group |

A group always keeps an owner: when dropping its last direct owner would leave it with no direct
owner and no owner metagroup, the backup adds the `thorium` user (the admin every deployment is
bootstrapped with) as the group's direct owner, adds the group to `thorium`'s groups, and lists the
addition. Since Thorium gives a group's owner no other role in it, `thorium` is then dropped from the
group's managers, users, and monitors if it was one of them, and each such drop is listed too. If
the backup holds no `thorium` user, the group's owners are kept and the problem still fails the
backup. Managers, users, and monitors may be left empty.

Without the flag, a scoped token's groups that are gone don't fail the backup: the token is backed
up and restored as it is.

`--prune-dangling` changes only the backup: the cluster's Redis keeps the dangling references until
you repair them or restore the backup. It never drops anything else, which still fails the `redis`
component:

- an image or pipeline naming a group the backup doesn't hold, and a pipeline running an image its
  group doesn't hold;
- two users sharing a primary token, and a scoped token holding a primary token's value or another
  scoped token's;
- a user, group, image, pipeline, scoped token, or network-policy set held twice or named with a
  `:` or an empty name, a network-policy set of a group the backup doesn't hold, and a scoped token
  whose owner isn't in the backup;
- a record that doesn't parse.

Nor does it look at references the API doesn't check at all: the rows of the Scylla tables (samples,
results, tags, comments, repos, entities, associations, and network policies, for example) naming a
group or user that is gone, notifications of images and pipelines that are gone, a network-policy
set naming a policy that has no row, and a group member whose own groups don't list the group.
These are backed up and restored as they are.

What a pruning backup changed is listed in `pruned.jsonl` in the backup directory, one JSON object
per change. Each change is logged at WARN when the `redis` component writes the file, and the run's
last line on stderr gives the count:

```json
{"record":"groups/analysts","field":"owners","action":"dropped","name":"old-admin","referent":"user","reason":"the user isn't in the backup and doesn't exist"}
```

| Field | What it holds |
|---|---|
| `record` | The record changed: `groups/<group>`, `users/<user>`, or `scoped_tokens/<owner>/<token name>` |
| `field` | The field changed: a group role (`owners`, `managers`, `users`, `monitors`) or `groups` |
| `action` | `dropped` (a name was removed), `added` (the `thorium` user was made the owner), or `deleted` (a scoped token left with no group was deleted) |
| `name` | The user or group name dropped or added; `null` for a deleted token |
| `referent` | `user` or `group`, the kind of record `name` names; `null` for a deleted token |
| `reason` | Why the change was made |

The file names records and users and groups, never a token value or another stored secret. The
manifest's `pruned` entry counts its lines and pins its sha256, and `backup verify` checks both. A
backup that pruned anything exits 2, like one with recorded failures, since it doesn't hold all the
cluster held, and a restore of it needs `--accept-failures`. When nothing was pruned there is no
`pruned.jsonl` and no `pruned` entry, and the run exits as it would without the flag.

### Accepting broken links

A broken link is a row or record that names an S3 object the cluster itself lacks: an entity's
image or a user's icon whose object was deleted from the graphics bucket, a sample or repo whose
object is gone from its bucket, a result whose file or entity blob is gone, or a comment whose
attachment is gone. By default the backup fails that object (a `failed` record, exit 2) and a
restore of the backup needs `--accept-failures`. When such links are known and harmless, pass
`--accept-broken-links` to go on without the problem being fatal to the restore:

```bash
thoradm backup new --accept-broken-links -o /mnt/backups/thorium-2026-10-17
```

With the flag, an S3 object that a backed-up row names and that S3 answers 404 for (`NoSuchKey`;
a missing bucket is still a failure):

- is not copied, since the cluster doesn't hold it, and the backup carries on with every other
  object;
- is recorded as a `broken-link` record in `failures.jsonl`, naming the component, the bucket and
  missing `key`, and the record that names it in `referrer` (`entities:<id>` for an entity's image,
  `users:<name>` for an icon, `s3_ids:<id>`, `results:<id>`, or `comments:<sha256>/<id>`);
- is kept as the cluster holds it: the row in the archives, and the user in `redis.json`, still
  name the missing key, and nothing is stripped (`--prune-dangling` is the option that strips
  references, and only user and group ones);
- is counted in the manifest's `broken_links`, apart from `failures` and `skipped`, and named in the
  run's last line on stderr;
- makes the run exit 2, as any record does.

Everything else is as without the flag. A failed object that is not a missing object (a timeout that
exhausted its retries, a key that can't be kept as a path, a row that can't be read) is still a
`failed` or `skipped` record. `--abort-on-failure` still stops the run at the first failed or skipped
object, but not at an accepted broken link. The flag doesn't cover the Redis state: a user or group
reference that names a user or group that is gone still fails the `redis` component, or is dropped
with `--prune-dangling`, since a restore refuses it.

What reads a backup with accepted broken links:

- `backup restore` needs no `--accept-failures` when the backup's only records are accepted broken
  links: it logs them as known, lists them in its confirmation and in the summary's `broken_links`,
  and writes the rows and the Redis state as the backup holds them, so the restored cluster has the
  same broken links the source had. A backup that also has failed or skipped objects, or pruned
  references, still needs `--accept-failures` for those, and the restore's summary lists the
  missing objects (`missing_from_backup`) apart from the links (`broken_links`).
- `backup verify` accepts them: each is a passing `broken-link` check and a WARN line, not a failed
  check, so a backup with only accepted broken links verifies with exit 0. It still fails a backup
  whose manifest count differs from the file.
- `backup scrub` reads only the table archives and is unaffected.
- A run that resumes an interrupted backup keeps accepting broken links when the interrupted run did
  (the marker's `accept_broken_links`) or when the resuming command passes the flag; objects the
  interrupted run recorded as failed are tried again under that rule.

A backup taken without the flag behaves as before.

## Reading the Results

### Exit codes

| Exit code | Meaning | What to do |
|---|---|---|
| 0 | The backup finished and nothing failed or was skipped | Nothing; the backup is complete |
| 2 | The backup finished, but objects failed or rows were skipped, each listed in `failures.jsonl` and counted in the manifest; or `--accept-broken-links` accepted broken links, each listed in `failures.jsonl` and counted apart in the manifest; or `--prune-dangling` dropped references, each listed in `pruned.jsonl` and counted in the manifest | Read `failures.jsonl` and `pruned.jsonl`. Fix the cause and take a new backup into a new directory. A backup with failed or skipped objects is missing those objects, and a restore of it, or of one that pruned references, needs `--accept-failures`; a backup whose only records are accepted broken links restores without it |
| 1 | The run stopped early: `--abort-on-failure` hit a failure, the directory was refused, a config or usage error, or a component couldn't run at all | Read the last error on stderr. If `backup-in-progress.json` is present, fix the cause and run the same command again to resume |
| 101 | Thoradm crashed (a panic) | Report it with the stderr output; the directory is an interrupted backup and can be resumed |

No count or threshold of failures stops a run; only `--abort-on-failure` does. The last line on
stderr says how the run ended.

### Reading `failures.jsonl`

`failures.jsonl` in the backup directory lists every object the run couldn't back up, and every
broken link it accepted with `--accept-broken-links`, one JSON object per line. Each record is also
logged on stderr as it happens. An empty file means nothing failed.

```json
{"kind":"failed","component":"s3-ids-objects","table":"s3_ids","bucket":"thorium-files","key":"498f10c3-9fcd-4701-8812-746d0b64b9e0","operation":"download","attempts":5,"error":"service error: NoSuchKey","at":"2026-10-17T23:55:38.111880138Z"}
```

| Field | Present | What it holds |
|---|---|---|
| `kind` | Always | `failed` (the object exhausted its retries or hit a permanent error), `skipped` (a row that couldn't be read into its archive struct), or `broken-link` (a reference to an S3 object the cluster lacks, accepted with `--accept-broken-links`) |
| `component` | Always | The component, as named on the command line (`redis`, `samples-list`, `result-files`, ...) |
| `table` | Rows, ranges, archives | The Scylla table |
| `bucket` | S3 objects | The S3 bucket |
| `token_range` | Ranges and rows | The token range `[start, end]` being read |
| `key` | When known | The partition key, S3 key, Redis file, or archive path, as far as it could be read |
| `object` | When known | A short name for the object (a sha256, an id, a partition) |
| `referrer` | Broken links | The record that names the missing object, as `<table or collection>:<id>`: `entities:<entity id>`, `users:<user name>`, `s3_ids:<id>`, `results:<result id>`, or `comments:<sha256>/<comment id>` |
| `operation` | Always | What failed: `read-range`, `deserialize-row`, `write-archive`, `read-archive`, `download`, `redis`, `verify` for a failed check of scrub or verify, and others for restore |
| `attempts` | Always | The attempts made, including the first |
| `error` | Always | The last error, on one line |
| `at` | Always | When the last attempt ended |

A failed range means the rest of that range's rows weren't read; a failed download means that object
isn't in the backup; a skipped row is a row whose stored values don't fit the archive format,
such as a comment whose attachments column isn't valid JSON, which skips that comment's attachments
and no others. When the `redis` component fails, no state names the users' icons, so the `graphics`
component writes one `skipped` record saying the icons weren't checked; without it, an
`--accept-broken-links` list could pass as whole when it isn't.
A user, group, image, pipeline, or scoped token the API can't parse, and records that don't fit
together (for example a group member that isn't a user, a user in a group that doesn't exist, or
two users sharing a token), fail the `redis` component with one record per Redis key, naming the
problem but never a stored value, and no `redis.json` is written; repair the records and back up
again into a new directory, or, for dangling user and group references, back up with
`--prune-dangling` (see [Pruning dangling references](#pruning-dangling-references)). A change
made while the API reads Redis can look like either problem (a user deleted between the API listing
the users and reading them reads as an empty record), so the API reads every record up to three
times, pausing a little longer after each read, and fails only if the last read still has problems.
A record that is really damaged therefore costs well under a second of extra reads before it fails
the backup.

## What Is Backed Up

Every resource Thorium stores is in thoradm's coverage registry, either backed up by a component or
excluded with a reason. The table below is generated from that registry. Run
`thoradm backup coverage` to print it from the installed binary (it needs no config file and no
connection to a cluster), or `thoradm backup coverage --json` for a JSON array with one object per
resource in the same order and the fields `store`, `name`, `prefixes`, `base_table`,
`classification`, `component`, `reason`, `category`, and `gap`. Developers regenerate the table with
`cargo run -p thoradm --example coverage_page`. Excluded resources are runtime state
that is invalid after a restore (reactions, jobs, workers, stage logs, events, caches, and the
`ephemeral` and `reaction_cache` buckets) or data rebuilt from other data. The "Known gap" column
lists what today's backup or restore doesn't carry yet. The Thoradm guide's
[What a Backup Holds](./thoradm.md#what-a-backup-holds) sums it up for restoring.

| Store | Resource | Classification | Component or reason | Category | Known gap |
|---|---|---|---|---|---|
| Scylla table | `samples_list` | Backed up | `samples-list` | CORE |  |
| Scylla table | `s3_ids` | Backed up | `s3-ids` | CORE |  |
| Scylla table | `comments` | Backed up | `comments` | CORE |  |
| Scylla table | `results` | Backed up | `results` | CORE |  |
| Scylla table | `results_stream` | Backed up | `results-stream` | CORE |  |
| Scylla table | `tags` | Backed up | `tags` | CORE |  |
| Scylla table | `repo_data` | Backed up | `repo-data` | CORE |  |
| Scylla table | `repos_list` | Backed up | `repo-list` | CORE |  |
| Scylla table | `commitishes` | Backed up | `commitish` | CORE |  |
| Scylla table | `commitish_list` | Backed up | `commitish-list` | CORE |  |
| Scylla table | `network_policies` | Backed up | `network-policies` | CORE |  |
| Scylla table | `entities` | Backed up | `entities` | CORE |  |
| Scylla table | `associations` | Backed up | `associations` | CORE |  |
| Scylla table | `notifications` | Backed up | `notifications` | CORE |  |
| Scylla table | `nodes` | Backed up | `nodes` | CORE |  |
| Scylla table | `logs` | Excluded | stage logs of reactions, which are runtime state and expire by TTL | EPHEMERAL |  |
| Scylla table | `events`, only in keyspaces older releases created | Excluded | a table of trigger events nothing reads or writes | EPHEMERAL |  |
| Scylla view | `samples` (view of `samples_list`) | Backed up | `samples-list` | CORE |  |
| Scylla view | `s3_sha256s` (view of `s3_ids`) | Backed up | `s3-ids` | CORE |  |
| Scylla view | `comments_by_id` (view of `comments`) | Backed up | `comments` | CORE |  |
| Scylla view | `results_auth` (view of `results_stream`) | Backed up | `results-stream` | CORE |  |
| Scylla view | `results_ids` (view of `results_stream`) | Backed up | `results-stream` | CORE |  |
| Scylla view | `tags_by_item` (view of `tags`) | Backed up | `tags` | CORE |  |
| Scylla view | `tags_case_insensitive` (view of `tags`) | Backed up | `tags` | CORE |  |
| Scylla view | `repos` (view of `repos_list`) | Backed up | `repo-list` | CORE |  |
| Scylla view | `committed_repo_data` (view of `commitish_list`) | Backed up | `commitish-list` | CORE |  |
| Scylla view | `network_policies_by_name` (view of `network_policies`) | Backed up | `network-policies` | CORE |  |
| Scylla view | `network_policies_default` (view of `network_policies`) | Backed up | `network-policies` | CORE |  |
| Scylla view | `entities_by_id` (view of `entities`) | Backed up | `entities` | CORE |  |
| Scylla view | `entities_by_name` (view of `entities`), only in keyspaces older releases created | Backed up | `entities` | CORE |  |
| Redis | `users` | Backed up | `redis` | CORE |  |
| Redis | `user_data:*` | Backed up | `redis` | CORE |  |
| Redis | `user_groups:*` | Backed up | `redis` | CORE |  |
| Redis | `users_token_map` | Backed up | `redis` | CORE |  |
| Redis | `users_email_map` | Backed up | `redis` | CORE |  |
| Redis | `oauth:alias_to_user:*` | Backed up | `redis` | CORE |  |
| Redis | `analysts` | Backed up | `redis` | CORE |  |
| Redis | `groups` | Backed up | `redis` | CORE |  |
| Redis | `groups_data:*` | Backed up | `redis` | CORE |  |
| Redis | `groups_direct:*` | Backed up | `redis` | CORE |  |
| Redis | `groups_metagroups:*` | Backed up | `redis` | CORE |  |
| Redis | `groups_combined:*` | Backed up | `redis` | CORE |  |
| Redis | `images:*` | Backed up | `redis` | CORE |  |
| Redis | `image_data:*` | Backed up | `redis` | CORE |  |
| Redis | `image_used_by:*` | Backed up | `redis` | CORE |  |
| Redis | `pipelines:*` | Backed up | `redis` | CORE |  |
| Redis | `pipeline_data:*` | Backed up | `redis` | CORE |  |
| Redis | `system_settings` | Backed up | `redis` | CORE |  |
| Redis | `scoped_token_map` | Backed up | `redis` | CORE |  |
| Redis | `scoped_tokens:*` | Backed up | `redis` | CORE |  |
| Redis | `netpols:*` | Backed up | `redis` | CORE |  |
| Redis | `netpol_used_by:*` | Excluded | rebuilt by the Redis restore from the restored images' network policies | DERIVED |  |
| Redis | `census:*` | Excluded | counts rebuilt from the Scylla tables by thoradm census, which restore runs | DERIVED |  |
| Redis | `system_info` | Excluded | re-initialized by restore | DERIVED |  |
| Redis | `system_keys` | Excluded | written at setup and read by nothing | EPHEMERAL |  |
| Redis | `reactions:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `reactions_group:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `reaction_data:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `reaction_jobs:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `reaction_tags:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `reaction_cache:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `sub_reactions:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `generators:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `stage_logs:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `logs:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `job_queue:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `job_data:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `workers:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `worker_data:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `streams:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `event-handler:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `search-streamer:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `cursors:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `image_ttc:*` | Excluded | runtime state of reactions, jobs, workers, or services, invalid after a restore | EPHEMERAL |  |
| Redis | `oauth:context:*` | Excluded | OAuth login contexts, which expire | EPHEMERAL |  |
| Redis | `oauth:registration_sessions:*` | Excluded | OAuth registration sessions, which expire | EPHEMERAL |  |
| Redis | `oauth:link_token:*` | Excluded | OAuth account-link tokens, which expire | EPHEMERAL |  |
| S3 bucket | `files` | Backed up | `s3-ids-objects` | CORE |  |
| S3 bucket | `repos` | Backed up | `s3-ids-objects` | CORE |  |
| S3 bucket | `attachments` | Backed up | `comment-attachments` | CORE |  |
| S3 bucket | `results` (`{id}/{file}`, `{id}/__thorium_entities/{kind}.json`) | Backed up | `result-files` | CORE |  |
| S3 bucket | `graphics` | Backed up | `graphics` | CORE |  |
| S3 bucket | `ephemeral` | Excluded | per-reaction scratch files | EPHEMERAL |  |
| S3 bucket | `reaction_cache` | Excluded | per-reaction cache files | EPHEMERAL |  |

A view isn't copied: restore rebuilds it from its base table, waiting for the build with no time
limit and logging which views are built and which are still building. Every column of a backed-up table is
in the backup, and the manifest lists, per table, the columns its archives carry. A row is backed up
whatever columns it leaves unset, and restore writes each unset cell back as a null, so it reads
back without a value, as in the source; a row whose stored values don't fit the archive format (for
example a notification level this thoradm doesn't know) is skipped and recorded instead.

### Archive formats

The archives' row layout has a format version, which the manifest records:

| Format | Written by | What it holds |
|---|---|---|
| 1 | thoradm before format 2, including every backup without a manifest | Every column but `results.tool_version` and `results.entities`, `results_stream.tool_version`, `repos_list.default_checkout` and `repos_list.earliest`, and `associations.submissions_source` and `associations.submissions_other`; no notification TTLs; a row with one of its required columns unset is skipped |
| 2 | This thoradm | Every column, each notification's remaining TTL, and every row, whichever regular columns it leaves unset |

A thoradm reads every format up to its own, so this thoradm restores format-1 and legacy backups as
before: the columns format 1 doesn't carry are left unset, and notifications take the table's
default TTL (the restore warns once). A `--merge` of a format-1 or legacy backup therefore leaves
those columns as the target holds them, rather than clearing them on the rows it writes. Such a
backup also holds no graphics and no result entity blobs, so the restore restores none, and it
warns once about them and the columns it lacks.

Restore a format-2 backup only with a thoradm that supports format 2, at least as new as the one
that took it. A thoradm that writes manifests but reads only format 1 refuses a format-2 backup
before it writes anything, naming the version that wrote it. A thoradm older than manifests (the
thoradm of earlier Thorium releases) does not refuse it: it replaces the namespace's Redis data with
the backup's `redis.json` and then fails on the Scylla data, leaving the tables unrestored.

## The Backup Directory

### An example

A finished backup of every component, after one `backup scrub` and one `backup verify`:

```text
thorium-2026-10-17/                           0700  the backup directory
├── backup.lock                               0600  the lock a running backup holds
├── manifest.json                             0600  what the backup holds; written last
├── failures.jsonl                            0600  what the run couldn't back up (empty here)
├── redis.json                                0600  users, groups, images, pipelines, settings
├── pruned.jsonl                              0600  only with --prune-dangling: what it dropped
├── samples_list/                             0700  one directory per table
│   ├── data/
│   │   └── 6f1c...e2a9                       0600  archived rows, partition after partition
│   └── maps/
│       └── 6f1c...e2a9.thoriummap            0600  one 96-byte entry per partition
├── s3_ids/
│   ├── data/ ...
│   ├── maps/ ...
│   ├── objects.index.jsonl                   0600  one line per object copied
│   └── objects/
│       ├── files/498f10/498f10c3-...-746d0b64b9e0      0600  a sample's object
│       └── repos/1a2b3c/1a2b3c4d-...                   0600  a repo's object
├── comments/
│   ├── data/ ...
│   ├── maps/ ...
│   ├── objects.index.jsonl
│   └── objects/deb340/deb34065...cdffc/<comment id>/<attachment id>
├── results/
│   ├── data/ ...
│   ├── maps/ ...
│   ├── objects.index.jsonl
│   └── objects/fb088f/
│       ├── fb088fa1-...-43cdd4b668e6/nested/out.txt                  0600  a result file
│       └── fb088fa1-...-43cdd4b668e6/__thorium_entities/Device.json  0600  an entity blob
├── results_stream/ tags/ repo_data/ repos_list/ commitishes/ commitish_list/
├── network_policies/ entities/ associations/ notifications/ nodes/
├── graphics/                                 0700  the graphics bucket; no archives
│   ├── objects.index.jsonl
│   └── objects/
│       ├── users/users/thorium/icon.png                    0600  a user's icon
│       └── 3c9a51/3c9a5170-...-d02f1e/device.png           0600  an entity's image
└── reports/
    ├── scrub-20261017T235540.120Z/
    │   └── failures.jsonl                    0600  the scrub's own failures
    └── verify-20261017T235612.034Z/
        ├── failures.jsonl                    0600  every check verify failed
        └── verify.json                       0600  every check verify ran, with its counts
```

An interrupted backup has `backup-in-progress.json` instead of `manifest.json`, and may have a
`staging/` directory in an S3 component's directory holding a download that never finished.

### Every file and directory

The paths below start at the backup directory, `<backup>`. Placeholders:

- `<table>`: a backed-up table's name (`samples_list`, `s3_ids`, `comments`, `results`, ...; see
  [What Is Backed Up](#what-is-backed-up)).
- `<uuid>`: a random uuid naming one archive; its data file and map share it.
- `<role>`: the bucket role of an S3 ID's object: `files` for samples, `repos` for repos.
- `<prefix>`: the first 6 characters of the object's key, or of its first segment when that is
  shorter (`users` for `users/thorium/icon.png`), which spreads objects over directories.
- `<id>`, `<sha256>`, `<comment>`, `<attachment>`, `<result>`: the object's S3 id, the sample's
  sha256, the comment's id, the attachment's id, and the result's id.
- `<file>`: a result file's name, which may contain `/`.
- `<kind>`: an entity kind (`Device`, `Vendor`, ...).
- `<key>`: a graphics object's key, which may contain `/`.
- `<path>`: one or more path segments.
- `<operation>`: `backup`, `restore`, `scrub`, or `verify`; `<time>` is the UTC time the run started.

| Path | Kind | Mode | Written by | What it holds |
|---|---|---|---|---|
| `<backup>` | directory | 0700 | `backup new` | The backup |
| `<backup>/backup.lock` | file | 0600 | `backup new` | Empty. A running backup holds an exclusive lock on it; it stays after the run |
| `<backup>/backup-in-progress.json` | file | 0600 | `backup new`, at its start | Marks a backup that hasn't finished: `thoradm_version`, `started`, the `components` the run backs up, and `accept_broken_links`, whether the run accepts broken links (a run that resumes the backup keeps accepting them). Removed when the manifest is written |
| `<backup>/manifest.json` | file | 0600 | `backup new`, at its end | What a finished backup holds (see [The manifest](#the-manifest)). Its presence is what makes a backup finished |
| `<backup>/failures.jsonl` | file | 0600 | `backup new` | Every object the backup couldn't back up, one JSON line each (see [Reading `failures.jsonl`](#reading-failuresjsonl)); empty when nothing failed |
| `<backup>/redis.json` | file | 0600 | `backup new`, component `redis` | The Redis data the API serves to admins, as JSON: `settings`, `users`, `groups`, `images`, `pipelines`, `scoped_tokens`, and `network_policy_sets` (and `pruned`, with `--prune-dangling`) |
| `<backup>/pruned.jsonl` | file | 0600 | `backup new --prune-dangling`, component `redis` | Only when references were pruned: one JSON line per change made to the Redis state (see [Pruning dangling references](#pruning-dangling-references)) |
| `<backup>/<table>` | directory | 0700 | `backup new`, the table's component | One table's archives, and for `s3_ids`, `comments`, and `results`, its component's objects |
| `<backup>/<table>/data` | directory | 0700 | `backup new` | The table's archive data files |
| `<backup>/<table>/data/<uuid>` | file | 0600 | `backup new` | Archived rows: partitions one after another, each the rows that share a partition key |
| `<backup>/<table>/maps` | directory | 0700 | `backup new` | The table's archive maps |
| `<backup>/<table>/maps/<uuid>.thoriummap` | file | 0600 | `backup new` | One 96-byte entry per partition of `data/<uuid>`, in order (see [Archives](#archives)) |
| `<backup>/<table>/objects.index.jsonl` | file | 0600 | `backup new`, an S3 component | One JSON line per object copied (see [Objects](#objects)) |
| `<backup>/<table>/objects` | directory | 0700 | `backup new`, an S3 component | The copied objects |
| `<backup>/<table>/objects/<path>` | directory | 0700 | `backup new`, an S3 component | The directories of the object layouts below |
| `<backup>/s3_ids/objects/<role>/<prefix>/<id>` | file | 0600 | `backup new`, `s3-ids-objects` | A sample's object (from the `files` bucket) or a repo's (from the `repos` bucket), keyed by its S3 id |
| `<backup>/comments/objects/<prefix>/<sha256>/<comment>/<attachment>` | file | 0600 | `backup new`, `comment-attachments` | A comment attachment |
| `<backup>/results/objects/<prefix>/<result>/<file>` | file | 0600 | `backup new`, `result-files` | A result file |
| `<backup>/results/objects/<prefix>/<result>/__thorium_entities/<kind>.json` | file | 0600 | `backup new`, `result-files` | The entity blob of one kind of entity the result found |
| `<backup>/graphics` | directory | 0700 | `backup new`, `graphics` | The `graphics` component's objects and index; it holds no archives. An interrupted backup may also hold `graphics/staging/`, laid out like a table's `staging` below |
| `<backup>/graphics/objects.index.jsonl` | file | 0600 | `backup new`, `graphics` | One JSON line per object copied (see [Objects](#objects)) |
| `<backup>/graphics/objects` | directory | 0700 | `backup new`, `graphics` | The copied graphics |
| `<backup>/graphics/objects/<path>` | directory | 0700 | `backup new`, `graphics` | The directories of the layout below |
| `<backup>/graphics/objects/<prefix>/<key>` | file | 0600 | `backup new`, `graphics` | A user icon or an entity image |
| `<backup>/<table>/staging` | directory | 0700 | `backup new`, an S3 component | Downloads in progress. Created when the component starts and removed when it ends, so only an interrupted backup has it |
| `<backup>/<table>/staging/<sha256>` | file | 0600 | `backup new`, an S3 component | A download that hasn't finished, named by the sha256 of its bucket and key; it moves into `objects/` once complete. Restore never reads it, and a resumed run clears it |
| `<backup>/reports` | directory | 0700 | `backup new`, `backup restore`, `backup scrub`, `backup verify` | Reports of runs that read or resumed the backup |
| `<backup>/reports/<operation>-<time>` | directory | 0700 | `backup new` (`backup-`), `backup restore` (`restore-`), `backup scrub` (`scrub-`), `backup verify` (`verify-`, unless `--report-dir` names another directory) | One run's report |
| `<backup>/reports/<operation>-<time>/failures.jsonl` | file | 0600 | as above | For `backup-`, the failures of an interrupted run that a resumed run set aside; for `restore-`, `scrub-`, and `verify-`, that run's own failures |
| `<backup>/reports/verify-<time>/verify.json` | file | 0600 | `backup verify` | Every check verify ran, with how many objects it checked and how many failed (see [Checking a Backup](#checking-a-backup)) |

A crash in the middle of writing `manifest.json`, `backup-in-progress.json`, `redis.json`, or
`pruned.jsonl` can leave a `<name>.tmp` file beside it: each is written to a temporary file, synced, and renamed into
place, so the file itself is always whole or absent. The next write of that file removes it.

### The manifest

`manifest.json` is written last, once every component has run, so a directory without it isn't a
finished backup. A run stopped by `--abort-on-failure` doesn't write it. Its fields:

| Field | What it holds |
|---|---|
| `format_version` | The archive format version (2; see Archive formats above). A thoradm that reads manifests of an older version refuses a newer one, naming the thoradm version that wrote it; a thoradm older than manifests doesn't read it |
| `thoradm_version` | The version of thoradm that wrote the backup |
| `thoradm_commit` | The commit thoradm was built from, when the build recorded one; otherwise `null` |
| `namespace` | The Thorium namespace that was backed up |
| `partition_sizes` | The partition size of each partitioned data type in the cluster config when the backup ran (`associations`, `entities`, `events`, `files`, `repos`, `results`, `tags.entities`, `tags.files`, `tags.repos`) |
| `buckets` | The bucket name of each bucket role in the cluster config (`attachments`, `ephemeral`, `files`, `graphics`, `reaction_cache`, `repos`, `results`) |
| `started`, `finished` | When the run started and when the manifest was written (UTC) |
| `components` | The components the run backed up, with `all` and `no-s3` expanded |
| `failures` | How many objects failed across the run |
| `skipped` | How many rows were skipped across the run |
| `broken_links` | How many broken links the run accepted (`--accept-broken-links`), each a `broken-link` record in `failures.jsonl`; they are counted apart from `failures` and `skipped`, and `0` when the run accepted none |
| `failures_sha256` | The sha256 of `failures.jsonl` when the run recorded anything; otherwise `null` |
| `tables` | One entry per backed-up table, by table name: `archives` (each archive's `uuid`, `data_len` in bytes, map `entries`, `rows`, and `map_sha256`), `ranges` (each token range's `start`, `end`, and the `rows` read from it), the table's total `rows`, its `skipped` rows, and the `columns` its archives carry |
| `s3` | One entry per S3 component, by component name: the `bucket_roles` its objects came from, the `objects` copied and their total `bytes`, the object `index` file (relative to the backup directory), and its `index_sha256` |
| `redis` | When `redis` was backed up: the `len` and `sha256` of `redis.json`, and its record `counts` of `users`, `groups`, `images`, and `pipelines` |
| `siblings` | Other files in the backup directory that another feature wrote (`schema.json`), each with its `name`, `len`, and `sha256`; empty when there are none |
| `api_version_mismatch` | Only when `--allow-version-mismatch` let the backup of `redis` go ahead against an API of another version: `thoradm`, thoradm's version, and `api`, the version the API reported (`null` when it reported none). Absent otherwise |
| `pruned` | Only when `--prune-dangling` dropped references: the `count` of lines in `pruned.jsonl` and its `sha256`. Absent otherwise |

An abridged manifest:

```json
{
  "format_version": 2,
  "thoradm_version": "1.8.1",
  "thoradm_commit": null,
  "namespace": "thorium",
  "partition_sizes": { "associations": 180, "files": 180, "results": 60, "tags.files": 180 },
  "buckets": { "attachments": "thorium-attachments", "files": "thorium-files", "results": "thorium-results" },
  "started": "2026-10-17T23:55:37.940014185Z",
  "finished": "2026-10-17T23:55:38.111880138Z",
  "components": ["redis", "associations", "comment-attachments", "comments"],
  "failures": 0,
  "skipped": 0,
  "broken_links": 0,
  "failures_sha256": null,
  "tables": {
    "comments": {
      "archives": [
        { "uuid": "68ac6c38-e1a8-4283-845a-895bf1a4e08f", "data_len": 460, "entries": 2, "rows": 2,
          "map_sha256": "8c60740f71a1afdde26918ed93001e5ec74078bfca588f1add9707fe84c3bad1" }
      ],
      "ranges": [ { "start": -9223372036854775808, "end": -8070450532247928833, "rows": 0 } ],
      "rows": 2,
      "skipped": 0,
      "columns": ["group", "sha256", "uploaded", "id", "author", "comment", "files"]
    }
  },
  "s3": {
    "comment-attachments": { "bucket_roles": ["attachments"], "objects": 1, "bytes": 16,
      "index": "comments/objects.index.jsonl",
      "index_sha256": "f82c7bef7b8f0b7db21f2414defd65a575b81f307c7e7f8fbad63c2000bc8cb2" }
  },
  "redis": { "len": 26850, "sha256": "51731e4ae4e5e3aeb723ed708b8823b7e41f20698f8b82838115313e36000b1e",
    "counts": { "groups": 3, "images": 9, "pipelines": 3, "users": 3 } },
  "siblings": []
}
```

### Archives

Each table is read in token ranges by several workers; each worker writes its own archives. An
archive is a pair of files sharing a uuid:

- `data/<uuid>` holds partitions back to back. A partition is the rows of one partition key,
  serialized together with `rkyv` as a list of the table's archive struct.
- `maps/<uuid>.thoriummap` holds one entry per partition, in order. Each entry is exactly 96 bytes:
  the `rkyv` form of the partition's `start` and `end` byte offsets in the data file, a 64-bit hash
  of its partition key, and the sha256 (as 64 hex characters) of its bytes.

Each partition starts where the previous one ended, and the last one ends at the data file's length.
A data file is closed and a new archive started once it holds 10 GiB. `thoradm backup scrub`
re-hashes every partition and compares it with its map entry. A map that ends part way through an
entry, an entry that doesn't start where the previous one ended, or an entry past the end of its
data file is read as corruption, naming the file and the byte.

### Objects

Each S3 component copies its objects under its table's directory (`graphics` under `graphics/`), in
the layouts listed above, and writes `objects.index.jsonl`, one line per object that landed:

```json
{"bucket":"thorium-attachments","key":"deb34065...cdffc/78320478-65bc-4b13-8ca3-3b121ce48cfc/39eaf706-ae9a-48ba-b5ac-06d0d80ca323","size":16,"etag":"\"9f1c...\"","sha256":"a1b2..."}
```

The fields are the object's `bucket` and `key`, its `size` in bytes, the `etag` S3 returned, and
the `sha256` of the bytes written to the backup. A download is written to `staging/` and moved
into `objects/` only once complete, so a file under `objects/` is always a whole object.

A backup keeps only each object's bytes, not its S3 metadata. A restore uploads each graphic with
the content type its key's extension names (`.bmp`, `.gif`, `.jpeg`, `.png`, `.svg`, `.mp4`, or
`.webm`, the extensions the API gives graphics of each type it accepts), so icons and entity
images are served with their type again. A graphic without one of those extensions, and every
object of the other components, is uploaded as `application/octet-stream`.

## Checking a Backup

`thoradm backup verify --backup <dir>` checks a backup against its manifest without touching the
cluster. It is read-only for the backup: it writes only its report, to
`reports/verify-<time>/` in the backup or to the directory `--report-dir` names. It checks:

| Check | Fails when |
|---|---|
| Manifest | `manifest.json` is missing (an interrupted backup, or one taken by an older thoradm), unreadable, or of a newer format version; or it names an object index other than `<component directory>/objects.index.jsonl`, or a sibling other than `schema.json` |
| Coverage | A component the manifest lists has no entry, a table's directory or one of its listed archives is missing, or a table holds an archive the manifest doesn't list |
| Map files | A map's length isn't a whole number of 96-byte entries or isn't the manifest's, or its sha256 differs from the manifest's |
| Data files | A data file's length differs from the manifest's, or from where its last partition ends |
| Partitions | A partition's sha256 differs from its map entry: every partition of every table |
| Rows | The rows an archive decodes to differ from the manifest's, or a partition doesn't decode |
| Objects | An object the index lists is missing or has another size or sha256, a file under `objects/` isn't in the index, an index line's key can't be kept as a path under `objects/` (an empty, `.`, or `..` segment), or the index's sha256 or counts differ from the manifest's |
| Links | A table or S3 component directory, its `maps/`, `data/`, or `objects/`, a file in them, an object index, `redis.json`, `pruned.jsonl`, the backup's own `failures.jsonl`, or a sibling is a symbolic link, or can't be read (for example a directory thoradm has no permission to read). A backup holds only real files and directories, so verify fails a link instead of following it, and restore doesn't read through one; restore records a path it can't read and goes on with the rest |
| `redis.json` | Its length or sha256 differs, it doesn't parse, or it holds another number of users, groups, images, or pipelines |
| Siblings | `schema.json` is missing, changed, or present without being listed |
| `pruned.jsonl` | Its sha256 differs from the manifest's `pruned` entry, it is missing while the manifest lists it, or it is present while the manifest doesn't |
| Recorded failures | The backup's own `failures.jsonl` differs from the manifest's sha256, or the manifest records failed or skipped objects: each is listed, since the backup doesn't hold it |
| Broken links | The manifest's `broken_links` differs from the number of `broken-link` records in `failures.jsonl`. A broken link the backup accepted passes as accepted, one `broken-link` check per record, and is logged at WARN; it is not a failed check |

Each failed check is a record in the report's `failures.jsonl`, named by the file or object it
concerns (with the `verify` operation), and `verify.json` there lists every check with how many
objects it checked and how many failed. Verify exits 0 when everything matches, 2 when it finished
with failed checks, and 1 when it couldn't check the backup's completeness (no manifest, an
unreadable one, or a newer format) or `--abort-on-failure` stopped it at its first failed check. A
backup without a manifest still gets its archives checked (map lengths, partition checksums, rows),
but its completeness can't be, so it never passes. When you copy or move a backup, copy its files
themselves (`cp -r`, `rsync -a`), not links to them: a backup holding a link fails verify. The
backup directory you pass to `--backup` may itself be a link. After a restore,
`thoradm backup verify --cluster` checks the restored cluster against the backup instead; see
[After a Restore](./thoradm.md#after-a-restore) in the Thoradm guide.

`thoradm backup scrub --backup <dir>` runs only the partition checks, for every table:
`samples_list`, `s3_ids`, `comments`, `results`, `results_stream`, `tags`, `repo_data`,
`repos_list`, `commitishes`, `commitish_list`, `nodes`, `network_policies`, `entities`,
`associations`, and `notifications`. It needs no manifest, writes its failures to
`reports/scrub-<time>/failures.jsonl` in the backup, takes `--abort-on-failure`, and exits 0, 1,
or 2 like a backup.

## How Restore Uses a Backup

`thoradm backup restore --backup <dir>` restores a backup into the cluster the cluster config names.
Before it writes anything, it runs a preflight. Each check below refuses the restore (exit 1) with a
`preflight` record in its `failures.jsonl` that names the check, and nothing in the target changes:

| Check | Refuses when | Override |
|---|---|---|
| Manifest | The backup is interrupted (`backup-in-progress.json` without `manifest.json`), its manifest is unreadable or of a newer format version, or it has neither file (a legacy backup, taken by a thoradm older than manifests) and `--legacy` wasn't given; or `--legacy` was given for a backup that isn't legacy, including one whose `manifest.json` was removed (it still holds `backup.lock`, which only a thoradm that writes manifests leaves, and its archives' format can't be known without the manifest) | `--legacy`, for a legacy backup only; nothing for the others |
| Version | Only when the backup holds `redis.json`, or with `--wipe <namespace>`: the API reports a Thorium version other than thoradm's, patch included, or none (see [Run thoradm at the API's version](#run-thoradm-at-the-apis-version)) | `--allow-version-mismatch` |
| Verify | `backup verify` finds any problem (see [Checking a Backup](#checking-a-backup)): a missing table directory or `redis.json`, a changed checksum, an unlisted archive or object. Each problem is a `verify` record above the refusal | None |
| Recorded failures | The backup's manifest records failed or skipped objects, which the backup doesn't hold, or references `--prune-dangling` dropped (its `pruned` entry). Each object, and the count of pruned references, is listed as a record above the refusal. Accepted broken links don't refuse: the restore logs them as known and its summary lists them | `--accept-failures` |
| Empty target | Any backed-up table the backup holds already has a row in the target, `nodes` aside, whose rows a restore keeps (buckets aren't examined) | `--merge`, or `--wipe <namespace>` |
| Wipe | Only with `--wipe <namespace>`, in place of the empty-target check: the namespace given isn't the one the cluster config names; the API serves another namespace, or doesn't say which; or the namespace isn't a keyspace the wipe may truncate in or holds none of the tables a backup holds (see [Merge or Wipe](./thoradm.md#merge-or-wipe)) | None |
| Views | A view the restore drops is already missing (as an interrupted restore leaves it) and no earlier `views.cql` holds its statement (see [Running a Restore](./thoradm.md#running-a-restore)) | None |
| Confirmation | No terminal is attached and `--confirm` wasn't given; or the restore isn't confirmed at the prompt; or, for a wipe on a terminal, the namespace isn't typed | `--confirm` (without a terminal) |

- **A legacy backup** (no manifest) restores only with `--legacy`. Its completeness can't be
  checked, so verify checks only its archives (map lengths, partition checksums, rows), the run
  warns, and the summary says it was restored as legacy. Backups taken by a thoradm older than
  manifests need the flag; a backup with a manifest, or an interrupted one, is refused with it.
- **A backup with recorded failures** (a backup that exited 2) restores only with
  `--accept-failures`, a plain switch with no value. The restore warns, and its summary lists every
  object the backup is missing (component, table or bucket, key). The restore's own exit code
  follows only its own records: 0 when it restored everything the backup holds.
- **A target that already holds data** restores only with `--merge`, which keeps what the target
  holds: rows and objects created after the backup survive, those deleted after it come back, and on
  a key both hold the backup's values win. Only the backed-up tables the backup holds are checked;
  Redis isn't, since the Redis restore always replaces the namespace's users, groups, images, and
  pipelines, and buckets aren't, so a target with empty tables restores without a flag even when
  its buckets hold objects.
- **A target that already holds data** can instead be replaced with `--wipe <namespace>`, which
  truncates the backed-up tables of the namespace the cluster config names once every other check
  passes, shows its plan, and asks for the namespace to be typed (or `--confirm` without a
  terminal); `--wipe` and `--merge` exclude each other. The wipe sends no S3 request: buckets are
  left as they are, so an object no restored row names may remain, and cleaning those up is the
  admin's job. Scylla keeps a snapshot of every
  table it truncates (`auto_snapshot`), which holds the wiped data and its disk, so the wipe frees no
  Scylla disk until you clear them; rows written shortly before the wipe can be missing from them,
  so they aren't a complete copy of the wiped data. The plan and the final report list these truncate snapshots
  with their size and print the command, `nodetool clearsnapshot <namespace>`, to run on every node
  once `verify --cluster` passes; thoradm never clears them (see
  [Merge or Wipe](./thoradm.md#merge-or-wipe)).
- **An API of another version** than thoradm's refuses a restore of a backup holding `redis.json`,
  and every `--wipe` restore, unless `--allow-version-mismatch` is given; the restore then warns and
  its summary's `warnings` names both versions. Run the thoradm of the API's version instead
  whenever you can.
- **An interrupted backup** has no override: finish it first by running the same `backup new` into
  it again.
- **Without a terminal** the restore needs `--confirm`.

Every restore replaces the namespace's Redis data with `redis.json`, deleting every key of the
namespace first, or with `--redis-flush-all` every key in the database (see
[Large restores: flushing the Redis database](#large-restores-flushing-the-redis-database)). The
restore's other flags (`--backup`, `--report-dir`, `--retries`,
`--s3-concurrency`, and `--abort-on-failure`), what it writes and in which order, its report, and
what to do after it are in the Thoradm guide's [Restoring a Backup](./thoradm.md#restoring-a-backup)
and [Reading the Results](./thoradm.md#reading-the-results).

### Large restores: flushing the Redis database

`--redis-flush-all` (off by default) changes how the Redis restore clears Redis before it writes
`redis.json`. By default the API deletes only the namespace's keys: it walks every key in the Redis
database and deletes those of the namespace, which takes about 1.5 s per million keys in the
database, whichever namespace they belong to (about 14 minutes for 570 million small keys, roughly
100 GB). With the flag the API runs `FLUSHDB ASYNC` on its Redis database instead, which takes about
0.1 s per million keys.

- **What it flushes.** Every key in the database the API uses (database 0; Thorium has no setting
  for another): the namespace's keys and every other namespace's keys in that database, and keys
  anything else stored there. It is not `FLUSHALL`: Redis's other databases are left alone, despite
  the flag's name.
- **When to use it.** For large restores into a Redis database dedicated to this Thorium namespace.
  When another namespace or application shares the database, leave the flag off.
- **What stays the same.** The backup is validated first, and a backup the API refuses leaves
  Redis untouched; the records are rebuilt in the same order (init, settings, users, scoped tokens,
  groups, images, pipelines, network-policy sets), and the census is rebuilt after the tables. It
  needs an admin in the Thorctl config and thoradm at the API's version, as every Redis restore
  does, and it has no effect on a backup without `redis.json`.
- **The confirmation** names the Redis host and namespace and says that every key in the database
  will be deleted, other namespaces' keys included; the summary's `warnings` says the database was
  flushed.
- **A failed restore with the flag** names the step that failed in its error, the flush included.
  A failed flush deletes nothing; when a later step fails, every key in the database is already
  gone, other namespaces' users included, and the summary's `warnings` says so. Bootstrap an admin
  with the API's secret key, as for any restore that fails before its users step, and run the
  restore again.
- **Memory is freed in the background.** `FLUSHDB ASYNC` returns at once and Redis frees the
  deleted keys' memory afterwards, so Redis's memory use drops over the following moments rather
  than when the restore's Redis step ends.
- **Pause writers first**, as for every restore (see
  [Running a Restore](./thoradm.md#running-a-restore)): a key written while the flush runs may be
  deleted with the rest.

```Bash
thoradm backup restore --backup /mnt/backups/thorium-2026-10-17 --wipe thorium --redis-flush-all
```
