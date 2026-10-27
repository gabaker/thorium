# Results and Provenance

Tool results are created when a `pipeline` is run on a target `file`. The running `pipeline` instance is called a
`reaction` and may involve running several tools on the target `file`. The analysis artifacts created
by each tool are then automatically stored in Thorium after each `pipeline` stage has completed. We organize tool
results based on the name of the tool rather than the name of the pipeline where that tool was run.

Tools may generate several types of result output, including renderable and downloadable formats. These artifacts
include:

- results: renderable data including basic text and JSON formatted tool output
- result-files: downloadable files produced by the tool and any tool results larger than 1MB
- children: unpacked or transformed files that Thorium treats like sample files due to potential maliciousness.

You can view or download results and child files with Thorctl or the API. Results are shown by tool name (or by
result key for tool and workflow runs, see [Results of a run](./runs.md#results-of-a-run)).

## Thorctl
---

You can download results for specific samples using Thorctl with the following command (results are written under
`./results` by default; change that with `-o/--output`):

```bash
thorctl results get --files <SHA256>
```

Download results for multiple samples by repeating `--files` or by passing a file of SHA256s, one per line, with
`--file-list`:

```bash
thorctl results get --files <SHA256-1> --files <SHA256-2>
thorctl results get --file-list ./sha256s.txt
```

If you want to download results for specific tools then you can use the following command:

```bash
thorctl results get --tools <TOOL> --tools <TOOL> --files <SHA256>
```

You can also get results for any samples with a certain tag with the following command:

```bash
thorctl results get --tags Dataset=Examples
```

The tool and tag flags can be set together to get the results of running a tool on samples with a particular
characteristic:

```bash
thorctl results get --tools analyzer --tags Packed=True
```

`--results-only` skips the result files, `--filter` and `--skip` take regular expressions that choose which result files
download, `-G/--results-groups` limits the groups the results come from, and `--post-processing` (`strip`, `split`, or
`full`) says what happens to the results' metadata. `-r/--repos` and `--include-repos` get the results of repos too.

The results of tool runs and workflow runs are listed by their result key, such as
`malware-triage#r3~<WORKFLOW INSTANCE ID>` for a workflow run; see
[Results of a run](./runs.md#results-of-a-run). `thorctl results show <REACTION>` prints the results of one run.
`thorctl results upload` uploads results you produced yourself (see `thorctl results upload --help` for the
directory layouts it accepts).

The number of results from which Thorctl downloads files is limited to prevent inadvertent massive download requests.
To change the limit, use the `--limit`/`-l` flag:

```bash
thorctl results get --tags Incident=10001234 --limit 100
```

### Children Files

Many tools will produce entirely new samples called `children files` that are saved in Thorium after the tool exits.
For example, an unpacking tool might remove protective/obfuscating layers of a given malware sample in order to unpack
the core payload and save it as a new sample in Thorium for further analysis. The sample that a tool was run on to
produce a child file its `parent file`. A child file's origin names its parent: `thorctl files describe <CHILD-SHA256>`
shows the origin and the parent's SHA256 (see [File Origins](./uploading_files.md#file-origins)).

## Provenance
---

Every reaction records, when it is created, where it came from and what it ran: its source (the workflow
instance, tool instance, or pipeline), its plan (each stage and the exact tool instance it runs), and for
workflow runs each agent's config hash and toolset digest and every tool instance the run used, with its version,
spec hash, and image. Read it with `thorctl runs describe <REACTION>` (see [Following runs](./runs.md#following-runs)).
A result stores the `tool` and `tool_version` that produced it, so a result can be traced to the version that wrote
it even after the tool changes. Provenance that Thorium keeps on entities, such as the tools that produced a
filesystem or a decompiled function, names tool instances by id; names stored before the upgrade to tools read back
as `legacy_tools` (see [Upgrading to Agentic Workflows](../admins/agentic/upgrading.md#what-changes-for-existing-data)).

## API
---

| Method | Route | Description |
|--------|-------|-------------|
| `GET` | `/api/files/results/<SHA256>` | A file's results (query: `tools`, `groups`, `hidden`) |
| `GET` | `/api/files/result-files/<SHA256>/<TOOL>/<RESULT ID>` | Download a result file |
| `GET` | `/api/repos/results/<URL>` | A repo's results |
| `GET` | `/api/repos/result-files/<URL>` | Download a repo's result file |

You can only read results in groups you belong to; a file or group you can't see is answered with `404`.
