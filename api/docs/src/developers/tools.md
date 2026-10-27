# Tools and Tool Instances

Thorium runs every analysis job as a **tool instance**. Tools replace images as the way to add
analysis tools: images can still be listed, described, and deleted, but no longer created or
updated (those requests return `410 Gone`). This page covers creating, publishing, changing,
inspecting, and deleting tools with Thorctl, and pointing pipeline stages at them. Running a tool
directly on files is covered in [Running Tools and Workflows](../users/runs.md), changing things in
an editor in [Editing Resources](./editing_resources.md), and moving whole sets of tools between
instances in [Toolboxes](./toolbox.md).

### Concepts
---

| Term | Description |
| ---- | ----------- |
| Tool | The editable root of a tool. It has a stable id, a name, a fixed kind, its groups, and its *latest* version |
| Tool Instance | One immutable version of a tool, named `<tool>@<version>`. Publishing a new spec creates a new instance; only its deprecated flag can change later |
| Version label | A label unique within the tool, such as `1.2.0` (semantic) or any custom label |
| Latest | The newest created instance that isn't deprecated, whatever the labels say. Pipeline stages, agents, and toolsets that follow a tool at latest run this instance |
| Spec hash | A content hash of the instance's spec. Two specs that mean the same thing hash the same however they were written, and publishing a spec identical to the latest one is refused with `409` |

Tools come in three kinds, set by the top-level key of their spec:

| Kind | What it is |
| ---- | ---------- |
| `Image` | A workload that runs in K8s, BareMetal, Windows, Kvm, or External: the settings an image used to hold, regrouped. Pipeline stages run Image tools |
| `Agentic` | A function a model can call, with a name, description, and input schema. It is backed by a Thorium MCP tool (`ThoriumMcp`), by a pinned Image tool instance (`Container`), or by a tool built into a harness (`HarnessNative`) |
| `Harness` | The program that runs an AI agent's model and tool loop in a workflow run. See [Harnesses](./harnesses.md) |

Each instance carries tags that users can't remove: `ToolId`, `ToolVersion`, `ToolKind`,
`ToolSpecHash`, and, for agentic tools, `ToolFunction`.

### Who can do what
---

- Creating a tool needs edit rights and the `Entities` allow flag in every group you create it in.
  Editing, publishing, and deprecating need the same rights in **all** of the tool's groups. Deleting
  needs the creator, an admin, or a group manager or owner in all of its groups. A tool that still
  has instances, or that an agent, toolset, pipeline stage, or another tool's spec references,
  can't be deleted (`409`, naming the referrers you can see).
- Tool instances always have their tool's groups; change groups on the tool. Widening a tool to a new
  group needs the network policies its instances name in that group (admins add missing ones
  automatically, other users are refused).
- A network policy that allows a tool by name allows every tool in the group with exactly that name,
  including one you create or rename later, from the scaler's next cache reload on. Policies that
  guard a security boundary should name tools by id (see
  [Tool Entries](../admins/network_policies/network_policies.md#tool-entries)).
- A non-default `security_context` in a workload can only be published by an admin, and host path
  volumes must be on the deployment's host path whitelist (see
  [Admin Command Line Tool](../admins/thoradm/thoradm.md)).
- **A spec's `env` values are hashed, immutable, and readable by every member of the tool's
  groups.** Never put secrets in them; use a K8s Secret volume instead.

### Creating and publishing
---

Create a tool and its first version from a YAML or JSON spec:

```bash
thorctl tools create --spec strings.yaml --name strings --group analysts --version 1.0.0 \
  --description "Extract printable strings"
```

Publish a new version of an existing tool (by name or id):

```bash
thorctl tools publish strings --version 1.1.0 --spec strings.yaml
```

**Protected names.** A new tool, toolset, workflow, or pipeline can't be named like the kind of thing
it is: `tool`, `tools`, `toolset`, `toolsets`, `tool-set`, `tool-sets`, `tool_set`, `tool_sets`, `workflow`, `workflows`, `pipeline`, and `pipelines` are refused (in any letter case) with a `400` that names the word and the
list, so `thorctl run <KIND> <NAME>` and the docs never have to tell a name from a kind. Names that
only contain a word, such as `tool-runner` or `my-pipeline`, are fine. Something that already has
one of these names keeps working: it reads, updates, publishes, runs, and deletes as before, and an
update that repeats its own name is accepted, but renaming anything to a protected name is refused.

`--group` can be repeated to create the tool in several groups, `--spec -` reads the spec from
stdin, and `--dry-run` checks the spec and prints what would be created without creating it.
`--tag KEY=VALUE` (repeatable) puts tags on the tool. A Harness tool's first version also takes
`--provides <INSTANCE_ID>` (a native tool version the harness provides), `--harness-env
VALUE[=VAR]`, and `--disable-model <MODEL_ID>`; they are refused for the other kinds before
anything is sent. Unknown keys in a tool spec are
refused, so a typo fails before anything is sent. For agentic tools, `create`, `publish`, and
`describe` print a portability report: warnings about how the tool's schema must be rewritten for
model APIs that don't support all of it. Thorium doesn't rewrite the schema; the harness calling
the model does.

Roll back by deprecating the newest version, which moves `latest` to the next newest one:

```bash
thorctl tools deprecate <TOOL_INSTANCE_ID>
```

A deprecated instance can't be newly pinned, but existing pins keep resolving to it.

#### An Image tool

An Image spec holds a workload: its environment (`K8s`, `BareMetal`, `Windows`, `Kvm`, or
`External`) with its scheduling settings, how the agent runs each job (`execution`), and the data
each job receives and produces (`data`). The fields inside them are the image settings described in
[Configuring Images](./configuring_images.md).

```yaml
Image:
  environment:
    K8s:
      image: registry.example.com/strings:1.0
      scheduling:
        resources: {cpu: 1000, memory: 512}   # millicpu and MiB
        spawn_limit: Unlimited                # or {Basic: 4}
  execution:
    args:
      command: [strings, "-n", "8"]
      output: {Kwarg: "--out"}
    timeout: 300
```

A tool's results, children, and filesystems dependency settings, under `data.dependencies`, name
the tools they take results from by id in a `tools` list (see
[Dependencies](./configuring_images.md#dependencies)):

```yaml
Image:
  data:
    dependencies:
      results:
        tools: [<TOOL_ID>]
        names: [report.json]
```

`environment`, and inside `scheduling` both `resources` (`cpu` and `memory`) and `spawn_limit`, are
required; everything else has a default.

#### An agentic tool backed by a Thorium MCP tool

```yaml
Agentic:
  function_name: get_sample_info
  model_description: Get a sample's metadata by SHA256
  input_schema:
    type: object
    properties:
      sha256: {type: string}
    required: [sha256]
  backing:
    ThoriumMcp:
      tool: get_sample
  timeout: 60
```

`tool` must be one of the [Thorium MCP server's tools](../admins/agentic/mcp.md#tools).

#### An agentic tool that runs an Image tool

A `Container` backing pins one Image tool instance and runs it as a tool run when a model calls it.
`inputs` maps the call's input onto the job: `Positional` appends the value, `{Kwarg: <key>}` passes
it under a key, and `{Switch: <flag>}` passes the flag when the value is `true`. `samples` names an
input property holding SHA256s to run on.

```yaml
Agentic:
  function_name: yara_scan
  model_description: Scan files with YARA rules
  input_schema:
    type: object
    properties:
      depth: {type: integer}
      recursive: {type: boolean}
      samples: {type: array, items: {type: string}}
  backing:
    Container:
      tool_instance: <IMAGE_TOOL_INSTANCE_ID>
      inputs:
        args:
          depth: {Kwarg: "--depth"}
          recursive: {Switch: "--recursive"}
        samples: samples
      outputs:
        max_result_bytes: 65536   # 0 = unlimited
  timeout: 600
```

Agentic specs are checked when they are published: `function_name` must start with a letter or
underscore and hold at most 64 letters, digits, `_`, or `-`; `model_description` can't be blank;
property names must be portable; the input and output schemas are limited in size (64 KiB by
default); and there may be at most 5 `input_examples`, each valid against the schema. `timeout` is
required for `ThoriumMcp` and `Container` backings. Optional fields include `title`,
`output_schema`, `capabilities` (`effects` from `WritesThorium`, `WritesExternal`, and
`Destructive`, where no effects means read-only; `idempotent`; `file_inputs`; `file_outputs`;
`open_world`), `result_format`, `requires_approval`, `strict`, `loading`, and `icons`. Harness-native
tools are covered in [Harnesses](./harnesses.md#native-tools).

### Changing tools
---

A tool has two things you can change: its **root** (name, description, groups, and tags), which is
edited in place, and its **versions**, which are immutable, so a change to a spec is published as a
new version. `tools update` and `tools edit` cover both. `update` never opens an editor; `edit`
opens one unless you give it a change on the command line (see
[Editing Resources](./editing_resources.md) for the flags they share).

#### Print a spec to start from

`tools spec` prints a version's spec in the form `create` and `publish` read, so it round-trips:

```bash
# the latest version, as YAML, to a file
thorctl tools spec strings --output strings.yaml
# an older version, as JSON on stdout
thorctl tools spec strings@1.0.0 --format json
```

#### Publish a new version

`tools publish` makes a new version from a spec file, or from the latest version with a few fields
changed:

```bash
# from a file, with an explicit label
thorctl tools publish strings --version 1.1.0 --spec strings.yaml
# from a file, raising the highest semantic version of the tool (patch, minor, or major)
thorctl tools publish strings --bump minor --spec strings.yaml
# from the latest version, changing fields by path
thorctl tools publish strings --from-latest --set Image.execution.timeout=600 --bump patch
# check a spec and print what would be published
thorctl tools publish strings --version 1.1.0 --spec strings.yaml --dry-run
```

`--set` only works with `--from-latest`, and `--spec -` reads the spec from stdin. A spec that
equals the latest version's is refused with `409`, because a version's spec hash is unique.

#### Edit a spec in your editor

```bash
# open the latest version's spec, then publish what you save as the next patch version
thorctl tools edit strings
# start from an older version, and choose the label
thorctl tools edit strings@1.0.0 --version 1.0.1
# raise the minor number instead of the patch number
thorctl tools edit strings --bump minor
# no editor: change the spec by path and publish it
thorctl tools edit strings --set Image.execution.timeout=600 --yes
```

The new version's label is the `--version` you give, else the highest semantic version of the
tool raised by `--bump` (patch by default). When the tool's latest label isn't a semantic version
(for example `latest`), a person at a terminal is asked for a label and a script is told to pass
`--version`. Editing a spec never changes the version it started from. If the spec you save is the
same as the version you started from, nothing is published.

#### Change the tool itself

`tools edit --root` edits the root in an editor, and `tools update` changes it with flags or
a document:

```bash
# open the root: name, description, groups, tags (and harness settings for a harness tool)
thorctl tools edit strings --root
# change it from flags
thorctl tools update strings --rename strings-ascii --add-group reversers --tag area=static
thorctl tools update strings -d "Extract printable strings" --remove-group analysts
# or from a document, a merge patch, or single fields
thorctl tools update strings --file root.yaml
thorctl tools update strings --patch '{"description": "ASCII strings"}'
thorctl tools update strings --set 'groups=[analysts,reversers]' --unset tags.area
thorctl tools update strings --add-group reversers --dry-run
```

The typed flags (`--rename`, `-d/--description`, `--clear-description`, `--add-group`,
`--remove-group`, `--tag`, `--remove-tag`) can't be combined with `--file`, `--patch`, `--set`, or
`--unset`. The root document holds `name`, `description`, `groups`, and `tags` (plus `harness_env`
and `disabled_models` for a Harness tool), and shows `*id*`, `*kind*`, and `*latest*` for context
only. The `submitter` tag that Thorium sets isn't shown and can't be removed. Widening a tool to a group needs the network policies its
instances name there (see [Who can do what](#who-can-do-what)).

For a Harness tool, `tools harness-env` and `tools harness-models` change the model values the
harness exports and the model ids it may not use, without publishing a version:

```bash
thorctl tools harness-env my-harness --set OpenAiApiKey=LITELLM_API_KEY
thorctl tools harness-models my-harness --disable-model gpt-5
```

#### Deprecate a version

`tools deprecate <TOOL_INSTANCE_ID>` moves a tool's latest off a version, and `--undo` restores
it. Use it to roll back a bad release without deleting anything.

### Deleting tools
---

```bash
# one version
thorctl tools delete strings@1.0.0
# a tool and every version of it, newest first
thorctl tools delete strings --all-versions
# see what would be deleted
thorctl tools delete strings --all-versions --dry-run
# no confirmation, for scripts
thorctl tools delete strings --all-versions --yes
```

A tool with versions is refused without `--all-versions`, and `--all-versions` can't be combined
with `TOOL@VERSION`. The confirmation needs a terminal unless you pass `--skip-confirm` (alias
`--yes`). If a version can't be deleted (for example a pipeline stage or a toolset pins it), the
command names that version and the referrers on one line and stops; the versions deleted before it
stay deleted, and so do all of them when the tool itself is then refused because an agent or
toolset follows its latest version. Deleting needs the rights listed in
[Who can do what](#who-can-do-what).

### Converting an image to a tool
---

`thorctl tools snapshot GROUP/IMAGE` creates a tool from a stored image the same way the upgrade
converts images, so the tool's spec hashes the same as the migrated one:

```bash
thorctl tools snapshot analysts/strings --dry-run
thorctl tools snapshot analysts/strings --name strings --version 1.0.0 --group analysts
```

`--name`, `--version`, and `--group` default to the image's own. An image whose dependency lists
name other images is refused with the reason and the remedy, because the converted lists would
come out empty; snapshot those images in dependency order through the upgrade, or recreate the
dependencies by hand. A tool that already has the name in the target groups is never replaced.

### Inspecting tools
---

```bash
# list the tools in your groups
thorctl tools get
# a tool's kind, groups, latest version, and number of versions
thorctl tools describe strings
# every version, newest first, with the latest one marked
thorctl tools versions strings
# a version's spec, in the form create and publish read
thorctl tools spec strings@1.1.0
# the instances of a tool, toolset, or workflow
thorctl instances get tool strings
# one instance, by id or name
thorctl instances describe strings@1.1.0
# the fields that differ between two instances of the same kind
thorctl instances diff strings@1.0.0 strings@1.1.0
```

The list and describe commands take `-o/--format` (`table`, `human`, `json`, or `yaml`), and most
also take `--watch`, `--wait-for <field>=<value>`, `--interval`, and `--timeout` to poll until a
condition holds.

### Pipelines run tools
---

Every pipeline stage is now backed by a tool, recorded in the pipeline's `stage_tools` map: a Tool
id follows the tool's latest version, and a Tool Instance id pins one version. When a reaction is
created, each stage is resolved to the tool instance it runs and recorded in the reaction's plan; the
reaction then runs that plan even if the pipeline changes. A stage tool must be an Image or Harness
tool or tool instance, and you must have the developer role for its workload's scaler.

- A pipeline created through the API, `thorctl pipelines create`, or `thorctl pipelines import` must
  give a tool for every stage, or it is refused (`400` from the API, or an error from Thorctl before
  any request).
- A reaction on a pipeline with a stage that has no tool is refused with `400`, so a pipeline the
  upgrade could not convert stops running until its stages are set.

#### Moving a pipeline to tools

The upgrade to this release converts existing images to tools, and pipeline stages to the tools
migrated from their images, through its upgrade steps (see
[Upgrading Thorium](../admins/deploy/upgrades.md#2026-10-v04) and
[Upgrading to Agentic Workflows](../admins/agentic/upgrading.md)). What the upgrade leaves
unconverted is listed in the step's report: an image that lacks a field the conversion needs, whose name another
tool already uses, or whose dependency lists name no migrated image, and every pipeline stage that
uses such an image. A pipeline converts as a whole or not at all, so a pipeline with one such stage
is left untouched, converted stages included, and none of its stages has a tool. For each image
you create the tool for:

1. Create an Image tool from the image's settings with `thorctl tools create --spec ...` (see
   [An Image tool](#an-image-tool)), or convert the stored image with `thorctl tools snapshot` (see
   [Converting an image to a tool](#converting-an-image-to-a-tool)).
2. Set the stage's tool:

   ```bash
   thorctl pipelines stage-tool <PIPELINE> <STAGE> <TOOL>
   ```

   `<TOOL>` is the tool's name or id, and the stage follows its latest version. Write
   `<TOOL>@<VERSION>` or add `--pin <TOOL_INSTANCE_ID>` to pin one version instead, and `-g/--group` when another of your
   groups has a pipeline with the same name. An unknown tool id is refused with `404`.

Set every stage of such a pipeline this way, the stages that would have converted included: the
migration only accepts the first instance it created at its own id, and a tool made with
`thorctl tools create` has a different one, so rerunning the migration doesn't set the stages and
isn't needed. Once every stage has a tool, reactions on the pipeline run again. Network policies
resolve a name in `allowed_tools` to the tools in the group with exactly that name, so naming a
tool exactly after the image it replaces keeps existing network policy `allowed_tools` rules
working.

### Images are read-only
---

- `POST /api/images/` and `PATCH /api/images/{group}/{image}` return `410 Gone` with a message that
  points to the tool routes and `thorctl tools`. Image reads, lists, and deletes keep working.
- `thorctl images import` and `thorctl images edit` therefore stop before opening an editor or pushing a
  container, with `Images can no longer be created or updated; use tools (thorctl tools
  create|edit|publish)`. Image bans are refused the same way. To copy tools between instances use
  [`thorctl toolbox`](./toolbox.md), which imports tools, and a legacy image toolbox is converted to
  tools as it is read.
- The Web UI's image and pipeline pages don't work with this release yet; use Thorctl.

### Using the API
---

Tools are entities of kind `Tool`; scripts call the entities routes, whose create is a multipart form
(the OpenAPI document `/api/entities/openapi.json` lists the fields, see the [REST API
Reference](../architecture/api.md#rest-api-reference)).

| Task | Route |
| ---- | ----- |
| Create or publish | `POST /api/entities/`, `PATCH /api/entities/{id}` |
| List, read, or delete | `GET /api/entities/`, `GET /api/entities/details/`, `GET`, `DELETE /api/entities/{id}` |
| List a tool's instances | `GET /api/entities/tools/{id}/instances` |
| Tag or untag | `POST`, `DELETE /api/entities/tags/{id}` |
| Run an instance | `POST /api/tools/instances/{id}/runs` |

Common errors: `401` when you lack edit rights or the `Entities` allow flag in a group, `404` when a group
or tool doesn't exist or isn't visible, `409` when publishing a spec identical to the latest one or deleting
a tool that still has instances, `410` for any write to an image, and `422` when a spec has the wrong shape
or an unknown key.
