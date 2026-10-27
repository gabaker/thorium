# Spawning Reactions

In Thorium terminology, a `reaction` is a unit of work where one or more tools run on some data within a `pipeline`.
Thorium allows for many tools to be strung together into sequential or parallel `stages` of
a `pipeline`. The process for adding tools to run within Thorium and building pipelines is covered in detail
within the [Developer chapters](../developers/developers.md).

Every pipeline stage now runs a [tool](../developers/tools.md). A reaction on a pipeline whose stages
don't all have a tool is refused until a developer sets them. To run a single tool version or an AI
workflow directly, see [Running Tools and Workflows](./runs.md).

## Create reactions with Thorctl
---
Thorctl spawns reactions for a single file or for many files at once. Use the following command to spawn a single reaction
on a specific file using the file's SHA256 hash:

```bash
thorctl reactions create --pipelines <PIPELINE_GROUP>/<PIPELINE> <SHA256>
```

The pipeline is given with `-p`/`--pipelines` as `<GROUP>/<PIPELINE>`. The group can be left off (`-p <PIPELINE>`) when no
other group you belong to has a pipeline with the same name, and the older `<PIPELINE>:<GROUP>` form is also accepted. To run
several pipelines, separate them with commas (`-p demo/test-pipeline,demo/other-pipeline`).

If you want to run a pipeline on files that have a specific tag or tags, add the `-t/--tags` flag and specify a tag in the format
`KEY=VALUE` as shown below:
```bash
thorctl reactions create --limit <LIMIT> -p <PIPELINE_GROUP>/<PIPELINE> --tags Dataset=Examples
```

To specify multiple tags, enter a `-t/--tags` flag for each tag:
```bash
thorctl reactions create --limit <LIMIT> -p <PIPELINE_GROUP>/<PIPELINE> --tags Tag1=Hello --tags Tag2=Goodbye
```

To limit the tag search to files in specific groups, add `-g/--groups` (this filters the files; it doesn't select the
pipeline's group):
```bash
thorctl reactions create --limit <LIMIT> -p <PIPELINE_GROUP>/<PIPELINE> --groups <FILE_GROUP> --tags Dataset=Examples
```

You can also watch the status of reactions using `--watch` or `-W`.

`thorctl reactions get`, `reactions describe`, `reactions logs`, `results get`, and `tokens get` and `tokens describe`
also take `--format` (`table`, `human`, `json`, or `yaml`; token values are redacted in every format) and `--watch`
(re-read and re-print every `--interval`, 2 seconds by default, until interrupted). Every one of them except
`reactions logs` also takes `--wait-for <FIELD>=<VALUE>`, which polls until the condition holds and then prints once
(`--timeout` defaults to 10 minutes, and the command exits with code 3 when it passes), for example
`thorctl reactions describe <REACTION> --wait-for status=Completed`.

```bash
$ thorctl reactions create -p demo/test-pipeline --watch 85622c435c5d605bc0a226fa05f94db7e030403bbad56e6b6933c6b0eda06ab5
CODE | PIPELINE                  | SAMPLES                                                          | ID                                   | MESSAGE                         
-----+---------------------------+------------------------------------------------------------------+--------------------------------------+----------------------------------
200  | test-pipeline              | 85622c435c5d605bc0a226fa05f94db7e030403bbad56e6b6933c6b0eda06ab5 | a0498ac4-42db-4fe0-884a-e28876ec3496 | -
-----+---------------------------+------------------------------------------------------------------+--------------------------------------+----------------------------------

	WATCHING REACTIONS	

STATUS       | PIPELINE                  | ID                                  
-------------+---------------------------+--------------------------------------
...
```

### Many files and repos
---

`thorctl reactions create` makes one reaction per file or repo. Choose the inputs by SHA256 (listed on the
command line or in `--file-list`), by repo (`-r/--repos`, `--repo-list`), or by a tag and date search
(`-t/--tags`, `-g/--groups`, `-s/--start`, `-e/--end`). Preview the choice with `--dry-run`, cap it with
`--limit` (a search with no `--limit` is capped at one million reactions; `--no-limit` removes the cap),
tag the reactions with `-T/--reaction-tags`, and group them under one batch name with `-b/--batch`:

```bash
# preview, then create one reaction per file in the list and follow them
thorctl reactions create -p demo/test-pipeline --file-list hashes.txt --dry-run
thorctl reactions create -p demo/test-pipeline --file-list hashes.txt --batch triage-2026-10 --watch
```

Arguments for the pipeline's tools can be added with `--positionals`, `--kwargs`, and `--switches`
(`--override-positionals`, `--override-kwargs`, and `--override-cmd` replace the tool's own, and
`--reaction-args-file` reads them from a JSON file). Reactions in a batch are listed with
`thorctl reactions get -g <GROUP> -t <BATCH>` and read with `thorctl reactions describe -t <BATCH>`
or `thorctl reactions logs -t <BATCH>`. To run a tool, a workflow, or a pipeline once per input with
`thorctl run <KIND> <NAME> --one-per-input`, see [Batch runs](./runs.md#batch-runs).

### Thorctl Run
The legacy form of `thorctl run` quickly creates a reaction, monitors its progress, and saves its results to disk. It is
still supported when its first word is not a kind (the kind forms `thorctl run tool|workflow|pipeline <NAME>` are
preferred, see [Running Tools and Workflows](./runs.md)):

```bash
thorctl run <PIPELINE> <SHA256|REPO>
```

A new pipeline can't be named `tool`, `workflow`, `pipeline`, or another [protected
name](../developers/tools.md#creating-and-publishing), so a first word that is a kind never means a new pipeline. A
pipeline that already has one of those names is started with `thorctl run pipeline <NAME>`.

Unlike `thorctl reactions create`, this form displays the stdout/stderr output of each stage in real time and
automatically save the results to disk, effectively emulating running the reaction locally on your machine. This might be
preferable to `thorctl reactions create` for running a quick, one-off reaction.

The status of a reaction and its lifetime are covered in [Reaction Status](../developers/reaction_status.md).

### Who can do this, and limits
---

You need to be an owner, manager, user, or analyst in the pipeline's group (monitors can't create
reactions), and reactions must be allowed in the group. A pipeline with a stage that has no tool, or
that is banned (see [Bans](../developers/bans.md)), is refused with `400`. Reactions are cleaned up two
weeks after they finish (see [Reaction Status](../developers/reaction_status.md)).

### Using the API
---

| Task | Route |
| ---- | ----- |
| Create one reaction | `POST /api/reactions/` |
| Create many reactions | `POST /api/reactions/bulk/` |
| List a pipeline's reactions | `GET /api/reactions/list/{group}/{pipeline}/` |
| List by status or tag | `GET /api/reactions/status/{group}/{pipeline}/{status}/`, `GET /api/reactions/tag/{group}/{tag}/` |
| Read, update, or delete one | `GET`, `PATCH`, `DELETE /api/reactions/{group}/{id}` |
| Fail one | `POST /api/reactions/handle/{group}/{id}/fail` |
| Read stage logs | `GET /api/reactions/logs/{group}/{id}/{stage}` |
