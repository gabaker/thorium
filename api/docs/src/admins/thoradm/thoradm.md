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

Thoradm backs up Thorium's data in Redis, Scylla, and S3: samples, repos, comment attachments,
results, tags, and metadata on Thorium nodes, among others. Elasticsearch is not backed up: it
only holds search data derived from Scylla, which the search streamer rebuilds by reindexing (see
[Reindexing Elasticsearch](../deploy/upgrades.md#reindexing-elasticsearch)). Take a backup before
upgrading Thorium or approving an upgrade step that changes data, so you can restore it if
needed. Also keep a copy of the `thorium-credentials` Secret and your values files (see
[Backups](../deploy/operate.md#backups)).

```Bash
thoradm backup -h
Backup a Thorium cluster

Usage: thoradm backup <COMMAND>

Commands:
  new      Take a new backup
  scrub    Scrub a backup for bitrot
  restore  Restore a backup to a Thorium cluster
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

### Creating a Backup

To take a backup, run the following command:

```Bash
thoradm backup new
```

Pass component names (`thoradm backup new redis results ...`) to back up only some of them, or
`no-s3` to skip everything stored in S3; run `thoradm backup new --help` for the full list.
You can provide the `--output/-o` flag to specify where to save the backup. Depending on the
size of your Thorium instance, the backup may be many TB in size, so choose a location suitable
to store that data.

```Bash
thoradm backup new --output /mnt/big-storage
```

If your Thorium instance is very large, the backup command could take many hours. Running it as
a background process or in something like a detached `tmux` session might be wise.

### Restoring a Backup

You can restore a Thorium backup with the following command:

```Bash
thoradm backup restore --backup <BACKUP>
```

As with taking a new backup, restoring a backup could take several hours depending on the size of
the backup. Bear in mind that **the restore will wipe all current data in Thorium and replace it with
the data to be restored.** You might want to verify the backup hasn't been corrupted in anyway before
restoring by running the command in the following section.

### Scrubbing a Backup

Thorium backups contain partitioned checksums that are used to verify the backup hasn't been corrupted
in some way overtime. You can recompute these checksums and verify the backup with the following command:

```Bash
thoradm backup scrub --backup <BACKUP>
```

Thoradm will break the backup into chunks, hash each chunk, and check that the hash matches the one that's
stored in the backup. If there are any mismatches, one or more errors will be returned, and you can be fairly
confident that the backup is corrupt. Restoring a corrupt backup could lead to serious data loss, so it's
important to verify a backup is valid beforehand.

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

A census counts Thorium's data in Scylla (tags, files, repos, and commitishes) and rebuilds the
counts Thorium caches in Redis for listing that data. Take a new one after restoring a backup or
when listings look wrong:

```Bash
thoradm census new [all|tags|tags-case-insensitive|files|repos|commitishes]...
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
