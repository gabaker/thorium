# Building Pipelines

### What are pipelines?
---
Pipelines are used to string together one or more Thorium tool(s) into a runnable analysis playbook. The simplest
possible Thorium pipeline would contain a single pipeline stage that would run a single Thorium tool. A more
complicated pipeline might consist of multiple stages each containing one or many tools. The stages of a pipeline are
executed sequentially where each tool within one stage must complete successfully before a subsequent stage can start.
The tools within a pipeline stage may be scheduled to run in parallel by the Thorium scheduler, depending on the
available resources. The following table describes the concepts related to how pipelines run:

| Term | Description |
| ---- | ---- |
| Tool | A runnable analysis tool, published as versions that each hold a runtime configuration (an Image spec for container and bare metal tools). |
| Pipeline | An executable playbook of analysis steps called stages, stages are executed sequentially. |
| Stage | A step in a pipeline, each stage can contain multiple tools that may run in parallel. |
| Reaction | An instance of a pipeline that runs in one of Thorium's execution environments. |
| Job | The execution of a single tool instance from a pipeline |

Every stage of a pipeline is now backed by a [tool](./tools.md): the pipeline's `stage_tools` map
names, for each stage, a tool (which follows its latest version) or a pinned tool instance. A
pipeline can't be created without a tool for every stage, and a reaction on a pipeline with a stage
that has no tool is refused, so set each stage's tool with:

```bash
thorctl pipelines stage-tool <PIPELINE> <STAGE> <TOOL> [--pin <TOOL_INSTANCE_ID>] [--group <GROUP>]
```

In `stage_tools` and in `<STAGE>`, a stage is one name in the pipeline's `order` (what was an image name
before tools): a step of the order that runs two names in parallel needs a tool for each of them.
(`pipelines create` takes them as `--stage-tool` flags and refuses a pipeline without them.)

See [Moving a pipeline to tools](./tools.md#moving-a-pipeline-to-tools). Pipelines are built and
changed with Thorctl or the API.

### Create a pipeline
---

Before you build a pipeline, you must have already created a tool for each of its stages in your group. If you have
not done that yet, you can read about the process on the [Tools and Tool Instances](./tools.md) page.

A pipeline's name can't be `pipeline`, `pipelines`, `tool`, `tools`, `toolset`, `toolsets`, `workflow`,
`workflows`, `tool-set`, `tool-sets`, `tool_set`, or `tool_sets` (see [Protected
names](./tools.md#creating-and-publishing)); `tool-pipeline` is fine. A pipeline that already has one of these
names keeps working and is run with `thorctl run pipeline <NAME>`.

`thorctl pipelines create` builds a pipeline from its stages and the tool that runs each one:

```bash
# one stage, run by the tool of the same name
thorctl pipelines create antivirus --group analysts --stage clamav --stage-tool clamav=clamav
# stages that run together are comma separated, and each --stage is one step in order
thorctl pipelines create static --group analysts \
  --stage unpack --stage strings,yara --stage report \
  --stage-tool unpack=unpacker --stage-tool strings=strings@1.1.0 \
  --stage-tool yara=yara-scan --stage-tool report=reporter \
  --sla 3600 --description "Unpack, scan, then report"
# check the pipeline and print it as YAML without creating it
thorctl pipelines create static --group analysts --spec static.yaml --dry-run
```

`--stage-tool STAGE=TOOL` makes the stage follow the tool's latest version and `STAGE=TOOL@VERSION`
pins that version. Every stage needs a tool, and Thorctl checks that before it sends anything. The
tool is looked up by name among the tools of the pipeline's group; a name that matches no tool or
more than one tool in the group is an error naming the stage and group (a tool id works too).
`--trigger NAME=JSON` (or `NAME=@FILE`) adds a trigger. `--spec FILE` reads a whole pipeline
document, the same one `pipelines edit` shows, with `-` reading stdin; the flags replace the spec's
order, SLA, and description, replace the tool of any stage they name, and add triggers. `-o json`
or `-o yaml` prints the stored pipeline after it is created, and `--dry-run` creates nothing.

### Change a pipeline
---

`pipelines update` changes a pipeline without an editor, and `pipelines edit` opens it in one (see
[Editing Resources](./editing_resources.md) for the flags the two share):

```bash
# replace the order: ';' separates steps and ',' separates stages that run together
thorctl pipelines update static --order 'unpack;strings,yara;report'
# add a stage, which needs its tool, then remove a stage and its tool
thorctl pipelines update static --order 'unpack;strings,yara;triage;report' --stage-tool triage=triager
thorctl pipelines update static --order 'unpack;strings,yara' --remove-stage-tool report
# other settings
thorctl pipelines update static --sla 7200 --description "Scan and report" --add-trigger 'on-new=@trigger.json'
thorctl pipelines update static --remove-trigger on-new
# repoint one stage at a pinned version, or back at the latest
thorctl pipelines update static --stage-tool strings=strings@1.0.0
thorctl pipelines update static --stage-tool strings=strings
# open it in your editor, or change it from a document
thorctl pipelines edit static
thorctl pipelines update static --file static.yaml --dry-run
```

The pipeline document holds `description`, `sla`, `order`, `triggers`, and `stage_tools`, where each
stage maps to `{tool: NAME, version: LABEL}` (`version: null` follows the latest). The group, name,
and bans aren't part of it. A stage you leave unchanged keeps its stored tool without a lookup, so a
deleted tool never blocks an unrelated edit. `--order` and the other typed flags can be combined with
`--file`, `--patch`, `--set`, and `--unset` (the document flags apply first). A new stage needs both
a place in `--order` and a `--stage-tool`; `--remove-stage-tool` only works together with an
`--order` that drops the stage, because the API refuses a stage with no tool.

`pipelines stage-tool` is the single-stage shortcut, and `--remove` removes the stage and its tool
together:

```bash
thorctl pipelines stage-tool static strings strings@1.1.0
thorctl pipelines stage-tool static strings strings --pin <TOOL_INSTANCE_ID>
thorctl pipelines stage-tool static report --remove
```

Removing the only stage of a pipeline is refused; delete the pipeline instead.
`pipelines edit` asks you to confirm the changes after an editor session, as every `edit` does, and
`-g/--group` or a second positional group picks the pipeline when other groups have one with the same
name.

### Run a pipeline
---

```bash
thorctl run pipeline static --sample <SHA256> --wait --logs
thorctl run pipeline analysts/static --file ./dropper.bin --wait --output ./static-out
```

`thorctl run pipeline` (also spelled `pipelines run`) takes the same flags as the tool and workflow kinds, including
`--one-per-input` for one run per file; see [Running Tools and Workflows](../users/runs.md) for the comparison. The
older `thorctl reactions create` and `thorctl run <PIPELINE> <SHA256|REPO>` still work.

### Import and export
---

`thorctl pipelines export` writes a pipeline's `stage_tools` as `{tool, version}` objects (names and
version labels, not ids), and `thorctl pipelines import` reads them back, checks that every stage in
the order has a tool before it sends any request, and looks each tool up by name in the target group.
A pipeline config with no tool for one of its stages fails the import naming the stages, and an export
from an older Thorctl has none, so add them with `pipelines create` or `pipelines stage-tool`. A tool
that doesn't exist in the target group is an error naming the tool and group, so create or
[import](./toolbox.md) the tools first. Images are no longer exported or imported by these
commands. See [Importing, Exporting, and Editing Images and Pipelines](./import_export.md).

### Inspect and delete a pipeline
---

```bash
# list the pipelines in your groups, then see one in full (add --format json for the raw document)
thorctl pipelines get
thorctl pipelines describe analysts/static
# delete one or more pipelines of a group (asks first at a terminal, --skip-confirm in scripts)
thorctl pipelines delete -g analysts static
```

Deleting a pipeline doesn't delete the reactions that ran it or their results.

### Who can do what
---

Creating, changing, and deleting a pipeline needs the `Developer` (or `Admin`) system role and the
`Owner`, `Manager`, or `User` role in the pipeline's group; users who are not developers can still run
existing pipelines (see [Roles and Permissions](../getting_started/roles_permissions.md)). Users change or delete only what they created in the group; owners and managers may change or delete
any pipeline of the group.

### Using the API
---

| Task | Route |
| ---- | ----- |
| Create a pipeline (answers `204`) | `POST /api/pipelines/` |
| Read a pipeline | `GET /api/pipelines/data/{group}/{pipeline}` |
| List a group's pipelines | `GET /api/pipelines/list/{group}/` (`/details/` for the full documents) |
| Update or delete a pipeline | `PATCH`, `DELETE /api/pipelines/{group}/{pipeline}` |
| Read pipeline notifications | `GET /api/pipelines/notifications/{group}/{pipeline}` |

Common errors: `400` when the request fails validation (for example a stage has no tool), `401` when you lack the
role in the group, `404` for a pipeline, group, or tool that doesn't exist or isn't visible to you, and
`409` when a pipeline with that name already exists in the group.

### Troubleshooting a Running Pipeline (Reaction)
---

So what do you do if your pipeline fails to run successfully after you set it up? The logs for the reactions that you run are saved by the Thorium Agent and uploaded to the API. These logs include debug info printed by the Agent as well as all the stdout and stderr produced by the tool the stage runs. Reaction logs are critical to help troubleshoot why your pipeline fails when it is scheduled and run.

If your pipeline is stuck in a `Created` state and appears to never be scheduled to run, you will want to check the tool each stage runs and validate its settings (see [Configuring Images](./configuring_images.md)). If your review doesn't find any issues, your local Thorium admins can look at the API and Scaler logs to provide additional debug info.

| Problem | Developer Action |
| ------- | ---------------- |
| Pipeline is never spawned. | Check the configuration of each stage's tool. It may be preventing Thorium from scheduling it. Verify that Thorium has enough resources to run every tool in your pipeline. For K8s tools, confirm the registry path of the container image is valid. |
| Pipeline fails when scheduled. | Check the reaction logs for the pipeline that failed. For a pipeline to succeed all stages of a pipeline must return successfully and pass back a success return code 0 to the Thorium agent. |
| Pipeline fails and logs a Thorium specific error. | Sometimes Thorium breaks, ask an admin for some assistance. |
| Pipeline completes, but no results are returned. | Check the tool's configuration. The agent must be told what paths your tool writes analysis artifacts into. If this path is wrong, the agent won't ingest any tool results. |