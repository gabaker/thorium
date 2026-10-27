# Uploading Files

Now that you have access to Thorium, you may want to upload some files and run analysis tools on them. Thorctl uploads
files and whole directory trees in parallel, which makes it the way to upload many files at once; the API route
described under [API](#api) does the same from your own scripts.

When uploading files there are several options you may set that are described below. `Groups` is the only required
field. If you are not yet a member of any groups then follow the steps in the
[Adding/Editing Groups](../getting_started/adding_editing_groups.md) section and come back afterward.

| Field | Description | Format/Accepted Values | Required |
| ----- | ----------- | ----------- | ----- |
| Groups | Limits who can see this file | One or more group names | yes |
| Description | A short text explanation of the sample and/or its source | Any valid UTF-8 formatted text | no |
| Tags | Key/value pairs to help locate and categorize files | Any key/value pair; both key and value are required | no |
| Origins | Specifies where a file came from | Downloaded, Transformed, Unpacked, Wire, Incident, or Memory Dump | no |

It is recommended that you provide origin information for any file(s) you upload whenever possible. A key feature of Thorium
is its ability to store origin information in a structured format and automatically translate that information into metadata tags.
Tags allow you to filter the files that you browse through when looking for a file. As a result, if you don't provide any origin
information, it may be difficult to locate your files at a later date.

## Thorctl
---
Thorctl will eagerly upload
multiple files in parallel by default, and specifying a directory to upload will recursively upload every file within
the directory tree. To upload a file or a folder of files, you can use the following command (using `--groups`/`-g`
to specify the groups to upload to):

```bash
thorctl files upload --groups <group> <files/or/folders>
```

If you have multiple files or folders to upload (e.g. `./hello.txt`, `/bin/ls`, and `~/Documents`), you can upload them all
in one command like so:

```bash
thorctl files upload -g example-group ./hello.txt /bin/ls ~/Documents
```

### Uploading to Multiple Groups

You can upload to more than one group by placing commas between each group:

```bash
thorctl files upload -g <group1>,<group2>,<group3> <file/or/folder>
```

Or by adding multiple `-g` or `--groups` flags:

```bash
thorctl files upload -g <group1> -g <group2> -g <group3> <file/or/folder>
```

### Uploading with Tags

You can also upload a file with specific tags with the `--file-tags` or `-T` flag:

```bash
thorctl files upload --groups <group> --file-tags Dataset=Examples --file-tags Corn=good <file/or/folder>
```

Because tags can contain any symbol (including commas), you must specify each tag with its own `--file-tags` or `-T` flag rather
than delimiting them with commas.

### Running Pipelines on Upload

To run one or more pipelines on every uploaded file as soon as it's uploaded, pass `-p`/`--pipelines`. Each pipeline
must include its group, written `<GROUP>/<PIPELINE>` (the older `<PIPELINE>:<GROUP>` form also works). Separate multiple
pipelines with commas:

```bash
thorctl files upload --groups <group> --pipelines <pipeline-group>/<pipeline> <file/or/folder>
thorctl files upload -g example-group -p static/scan,static/triage ./my-folder
```

Every pipeline is checked before anything is uploaded; a pipeline given without a group, one that doesn't exist, or one
that is banned stops the upload with an error. The reactions are created through the bulk route, up to 100 per request,
a chunk of files at a time while the upload continues, so uploading thousands of files does not send a request per file.

### Running Workflows and Tools on Upload

`--workflows` and `--tools` start a workflow run or a tool run on every uploaded file, the same runs that
[`thorctl run workflow` and `thorctl run tool`](./runs.md) start on files already in Thorium:

```bash
# a workflow (its latest instance) and a tool (its latest version) on every new file
thorctl files upload -g example-group --workflows malware-triage --tools strings ./my-folder
# a revision, a tool version, and the group to run in, written <GROUP>/<NAME>
thorctl files upload -g example-group --workflows example-group/malware-triage#r2 --tools strings@1.0.0 ./my-folder
# pipelines, workflows, and tools together, then wait for every run
thorctl files upload -g example-group -p static/scan --workflows malware-triage --tools strings --wait ./my-folder
```

A workflow is named by its name (its latest Ready instance), `<WORKFLOW>#r<REVISION>`, or an instance ID, and a tool by
its name (its latest version), `<TOOL>@<VERSION>`, or an instance ID. When the workflow or tool is in several of the
groups you upload to, name the group to run in as `<GROUP>/<NAME>`; the group must be one of the upload groups. Every
workflow and tool is looked up before anything is uploaded, so a name that doesn't exist or can't run on the files stops
the upload with an error. Both flags take comma separated lists and combine with `--pipelines`; each file gets one run
of each. They take no arguments for the tool or workflow, so a run uses the tool's or workflow's own settings; use
`thorctl run tool ... --arg` for runs that need arguments.

Runs start only for files that were uploaded by this command. A file that already exists in the groups is reported as
`409 Already Exists` and left alone, with no new run (start runs on existing files with `thorctl run ... --sample`).

The runs of one upload share a batch tag: `--batch <NAME>` names it, and one is generated (and printed first) when you
follow the runs or start a workflow or tool. `thorctl runs get --batch <NAME>` lists the runs and `thorctl runs cancel
--batch <NAME>` ends them.

Without a follow flag the command prints a line for every started run and exits as soon as every run is started.
`--wait` and `--watch` then follow every run (pipeline, workflow, and tool) the way [a batch of runs](./runs.md#batch-runs)
is followed: Thorctl prints a line each time a run changes status and ends with a summary of the runs by status.
`--wait` gives up after `--timeout` (10 minutes) and `--watch` never does; `--interval` sets the wait between reads. Ctrl-C
ends Thorctl with exit status 130 and leaves the started runs running (cancel them with their batch name).

The table has the usual upload line for each file and then one line for each run started on it, with the run's reaction
in the `SUBMISSION` column and what ran in `MESSAGE` (`workflow malware-triage`); a run that did not start shows `ERR` and
why. With `-o json` stdout holds one JSON line per file (`"type": "upload"`, with `status` `uploaded`, `exists`, `failed`,
or `dry-run`) when it is handled, and, after the runs were followed, one line per run (`"type": "run"` with the file's
`path` and `sha256`, the `kind`, `target`, `id`, `group`, `status`, and `error`). The batch name and the summary go to
stderr. With `--dry-run` nothing is uploaded or started: each new file's line lists the runs that would start.

| Exit code | Meaning |
| --------- | ------- |
| `0` | Every file was uploaded or already existed, every run started, and, when followed, every run Completed |
| `1` | A file failed to upload (an unreadable path or an API error other than `409`), a run failed or did not start, or a pipeline, workflow, or tool was refused before the upload |
| `3` | No upload or run failed but some runs had not finished before `--timeout` |

A failure in a file or a run never stops the others; `1` wins over `3`. Uploads that fail now exit `1` (they printed
the failure and exited `0` before). Files found by walking a directory upload up to `--workers/-w` at a time (10 by
default; files named one by one on the command line are uploaded one after another), and workflow and tool runs start at
most `--workers` at a time.

### Filtering Which Files to Upload

There may be cases where you want to upload only certain files within a folder. Thorctl provides the ability to either
inclusively or exclusively filter with regular expressions using the `--filter` and `--skip` flags, respectively.
For example, to upload only files with the `.exe` extension within a folder, you could run the following command:

```bash
thorctl files upload --groups example-group --filter .*\.exe ./my-folder
```

Or to upload everything within a folder except for files starting with `temp-`, you could run this command:

```bash
thorctl files upload --groups example-group --skip temp-.* ./my-folder
```

Supply multiple filters by specifying filter flags multiple times:

```bash
thorctl files upload --groups example-group --filter .*\.exe --filter .*evil.* --skip temp-.* ./my-folder
```

The filter and skip regular expressions must adhere to the format used by the Rust
[regex crate](https://docs.rs/regex/latest/regex/#syntax). Fortunately, this format is very similar to
most other popular regex types and should be relatively familiar. A helpful site to build and test your
regular expressions can be found here: [https://rustexp.lpil.uk](https://rustexp.lpil.uk/)

#### Hidden Directories

Additionally, if you want to include hidden sub-directories/files in a target directory, use the `--include-hidden` flag:

```bash
thorctl files upload -g example-group ./files --include-hidden
```

### Folder Tags

Thorctl also has a feature to use file subdirectories as tag values with customizable tag keys using the `--folder-tags` option.
For example, say you're uploading a directory `foo` with the following structure:

```
foo
├── foo1.txt
└── bar
    ├── bar1.txt
    ├── bar2.txt
    ├── baz
        └── baz1.txt
    └── qux
        └── qux1.txt
```

The `foo` directory contains five total files spread across three subdirectories. Each tag we provide with `--folder-tags`
corresponds to a directory from top to bottom (including the root `foo` directory). So for example, if you run:

```bash
thorctl files upload -g example-group foo --folder-tags alpha/beta/gamma
```

The key `alpha` would correspond to the `foo` directory, `beta` to `bar`, and `gamma` to `baz` and `qux`. So all
files in the `foo` directory **including files in subdirectories** would get the tag `alpha=foo`, all files in the
`bar` directory would get the tag `beta=bar`, and so on. Below is a summary of the files and the tags they
would have after running the above command:

| File | Tags |
| ---- | ---- |
| foo1.txt | `alpha=foo` |
| bar1.txt | `alpha=foo`, `beta=bar` |
| bar2.txt | `alpha=foo`, `beta=bar` |
| baz1.txt | `alpha=foo`, `beta=bar`, `gamma=baz` |
| qux1.txt | `alpha=foo`, `beta=bar`, `gamma=qux` |

A few things to note:

- Because `/` is used to delimit individual folder tag keys, keys cannot have `/` in them.
- Tags correspond to subdirectory *levels*, not individual subdirectories, meaning files in subdirectories on the same
level will get the same tag key (like `baz` and `qux` above).
- You don't have to provide the same number of tags as subdirectory levels. Any files in subdirectories deeper than the
number of folder tags will receive all of their parents' tags until the provided tags are exhausted (e.g. if `qux` had
a child directory called `quux` with a file called `quux1.txt`, it would get tags for `foo`, `bar`, and `qux`, but not
for `quux`).
- To skip a directory level, you can simply provide an empty tag key for that level. For example, `--folder-tags a//c`
on file `a/b/c/d.txt` would get the tags `a=a` and `c=c` but no tag for `b`.
- The root `/` directory in absolute paths as well as `.` in relative paths are ignored, so `--folder-tags a/b`
on `/a/b/c.txt`, `./a/b/c.txt`, and `./a/./b/./c.txt` would all have tags `a=a` and `b=b`. Note that `..` components
(e.g. `a/b/../b/c.txt`) are *not* ignored.

### Adjust Number of Parallel Uploads

By default, Thorctl can perform a maximum of 10 actions in parallel at any given time. In the case of file uploads, that means
a maximum of 10 files can be uploaded concurrently. You can adjust the number of parallel actions Thorctl will attempt to conduct
using the `--workers/-w` flag:

```bash
thorctl --workers 20 files upload --groups <group> <files/or/directories>
```

### Description and Origins

`thorctl files upload` has no flags for a description or an origin. Thorctl uploads files with the groups and tags
you give it; to set a description or a [file origin](#file-origins) at upload time, use the API. You can add tags to
files after they are uploaded with [`thorctl tags add`](./tagging_files.md#editing-tags-thorctl).

### Run Pipelines

`--pipelines`, `--workflows`, and `--tools` (see [Running Pipelines on Upload](#running-pipelines-on-upload) and
[Running Workflows and Tools on Upload](#running-workflows-and-tools-on-upload)) start a run on every uploaded file.
You can also run pipelines, tools, and workflows on files that are already in Thorium (see
[Spawning Reactions](./spawning_reactions.md) and [Running Tools and Workflows](./runs.md)).

## API
---

Thorctl uploads through `POST /api/files/`, a `multipart/form-data` request authenticated like every other route (see
[Login](../getting_started/login.md)). The form fields are:

| Field | Meaning |
| ----- | ------- |
| `groups` | A group to upload to (repeat the field for several groups); required |
| `description` | The sample's description |
| `tags[<KEY>]` | A tag value for `<KEY>` (repeat the field for several values) |
| `origin[...]` | The origin: `origin[origin_type]` names the type (`Downloaded`, `Transformed`, `Unpacked`, `Wire`, `Incident`, `MemoryDump`, `Source`, `CarvedPcap`, `CarvedUnknown`, or `None`) and the other `origin[<subfield>]` fields hold the subfields in the tables below |
| `data` | The file itself; put it after the other fields |

```bash
curl -H "Authorization: Bearer $TOKEN_B64" https://<URL>/api/files/ \
  -F groups=example-group -F 'tags[Dataset]=Examples' \
  -F 'origin[origin_type]=Downloaded' -F 'origin[url]=https://example.com/a.exe' \
  -F data=@./a.exe
```

Uploading a file you already uploaded to the same groups is refused with `409` and adds nothing; a group you aren't a
member of with `404`; a malformed form or an invalid value with `400`. The full request is in the
[REST API Reference](../architecture/api.md#rest-api-reference) served by your Thorium instance.


## File Origins
---

File `Origins` are the single most important piece of information in describing, locating, and understanding relationships
between files. Described below are all the options for file origins and their respective subfields.

#### Downloaded

The "Downloaded" Origin specifies that the file was downloaded from a specific URL.

| Subfield | Description | Format/Accepted Values | Required |
| ----- | ----------- | ----------- | ----- |
| URL | The URL the file was downloaded from | A valid URL | yes |
| Site Name | The name of the website the file was downloaded from | Any UTF-8 formatted text | no |

#### Transformed

The "Transformed" Origin specifies that the file is a result of transforming another file, whether by a tool or some other means.

| Subfield | Description | Format/Accepted Values | Required |
| ----- | ----------- | ----------- | ----- |
| Parent | The SHA256 of the original file that was transformed to produce this file | A valid SHA256 of an existing file in Thorium<sup>1</sup> | yes |
| Tool | The tool that was used to produce this transformed file | Any UTF-8 formatted text | no |
| Flags | The tool command-line flags that were used to transform this sample | One or more hyphenated alphanumeric flags<sup>2</sup> | no |

1. Your account must have access to the parent file in order to specify it in a file's origin
2. Example: `--flag1, --flag2, --flag3, -f`

#### Unpacked

The "Unpacked" Origin specifies that the file was unpacked from some other file, whether by a tool or some other means.

| Subfield | Description | Format/Accepted Values | Required |
| ----- | ----------- | ----------- | ----- |
| Parent | The SHA256 of the original file that this file was unpacked from | A valid SHA256 of an existing file in Thorium<sup>1</sup> | yes |
| Tool | The tool that was used to unpack this file | Any UTF-8 formatted text | no |
| Flags | The tool command-line flags that were used to unpack this sample | One or more hyphenated alphanumeric flags<sup>2</sup> | no |

1. Your account must have access to the parent file in order to specify it in a file's origin
2. Example: `--flag1, --flag2, --flag3, -f`

#### Wire

The "Wire" Origin specifies that a file was captured/sniffed "on the wire" en route to a destination.

| Subfield | Description | Format/Accepted Values | Required |
| ----- | ----------- | ----------- | ----- |
| Sniffer | The sniffer<sup>1</sup> used to capture this file | Any UTF-8 formatted text | yes |
| Source | The source IP/hostname this file came from when it was sniffed | Any UTF-8 formatted text | no |
| Destination | The destination IP/hostname where this file was headed to when it was sniffed | Any UTF-8 formatted text | no |

1. Example: `wireshark`

#### Incident

The "Incident" Origin specifies that the file originated from a specific security incident.

| Subfield | Description | Format/Accepted Values | Required |
| ----- | ----------- | ----------- | ----- |
| Incident ID | The name or ID identifying the incident from which the file originated | Any UTF-8 formatted text | yes |
| Cover Term | An optional term for the organization where an incident occurred | Any UTF-8 formatted text | no |
| Mission Team | The name of the mission team that handled the incident | Any UTF-8 formatted text | no |
| Network | The name of the network where the incident occurred | Any UTF-8 formatted text | no |
| Machine | The IP or hostname of the machine where the incident occurred | Any UTF-8 formatted text | no |
| Location | The physical/geographical location where the incident occurred | Any UTF-8 formatted text | no |

#### Memory Dump

The "Memory Dump" Origin specifies that the file originated from a memory dump.

| Subfield | Description | Format/Accepted Values | Required |
| ----- | ----------- | ----------- | ----- |
| Memory Type | The type of memory dump this file originated from | Any UTF-8 formatted text | yes |
| Parent | The SHA256 of the memory dump file in Thorium from which this file originates | A valid SHA256 of an existing file in Thorium<sup>1</sup> | no |
| Reconstructed | The characteristics that were reconstructed in this memory dump | One or more UTF-8 formatted strings | no |
| Base Address | The virtual address where the memory dump starts | An alphanumeric memory address | no |

1. Your account must have access to the parent file in order to specify it in a file's origin

#### Carved

The "Carved" Origin specifies that a file was "carved out" of another file (e.g. archive, memory dump, packet capture, etc.).
Unlike "Unpacked," "Carved" describes a sample that is a simple, discrete piece of another file. It's extraction can be easily
replicated without any dynamic unpacking process.

| Subfield | Description | Format/Accepted Values | Required |
| ----- | ----------- | ----------- | ----- |
| Parent | The SHA256 of the original file that was carved to produce this file | A valid SHA256 of an existing file in Thorium<sup>1</sup> | yes |
| Tool | The tool that was used to produce this transformed file | Any UTF-8 formatted text | no |
| Carved Origin | The type of file this sample was carved from (and other related metadata) | See below Carved origin subtypes | no |

1. Your account must have access to the parent file in order to specify it in a file's origin

Carved origins may also have an optional subtype defining what type of file the sample was originally carved from. The Carved
subtypes are described below:

##### PCAP

The "Carved PCAP" Origin specifies that a file was "carved out" of a network/packet capture.

| Subfield | Description | Format/Accepted Values | Required |
| -------- | ----------- | ---------------------- | -------- |
| Source IP | The source IP address this file came from | Any valid IPv4/IPv6 | no |
| Destination IP | The destination IP address this file was going to | Any valid IPv4/IPv6 | no |
| Source Port | The source port this file was sent from | Any valid port (16-bit unsigned integer) | no |
| Destination Port | The destination port this file was going to | Any valid port (16-bit unsigned integer) | no |
| Protocol | The protocol by which this file was sent | "UDP"/"Udp"/"udp" or "TCP"/"Tcp"/"tcp" | no |
| URL | The URL this file was sent from or to if it was sent using HTTP | Any UTF-8 formatted text | no |

##### Unknown

The "Carved Unknown" Origin specifies that a file was "carved out" of an unknown or unspecified file type.

This origin has no other subfields except for the ones from it's parent "Carved" origin.
