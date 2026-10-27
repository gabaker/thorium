# Toolboxes

### What is a toolbox?
---

A Thorium *toolbox* is a portable collection of everything needed to run analysis: **tools** (Image,
Agentic, and Harness, each with its versions and specs), **toolsets**, **AI agents**, **workflows**,
and **pipelines** whose stages name tools, plus the groups and network policies they use. A toolbox is
described by one `toolbox.json` manifest. It refers to everything by name, never by id, so the same
file can be imported into any Thorium instance, and container images stay in their registry: a tool's
spec names its image, and the manifest separately records how to build it.

Toolboxes come in two formats:

- **Format 2** (the current format, `"format_version": 2`) holds tools, toolsets, agents, workflows,
  pipelines with stage tools, and base builds. It is what `export` writes and what `build` writes for
  tool manifests.
- **The legacy format** holds `images` and `pipelines` (what toolboxes used to carry). Every toolbox
  command still reads it: the command converts it to tools in memory, reports what the conversion had to
  skip or change, and carries on. `toolbox convert` writes the converted file once so you don't have to
  convert on every read.

`thorctl toolbox` has a command for each step of a toolbox's life:

| Command | Purpose |
| ------- | ------- |
| `thorctl toolbox validate` | Check a toolbox offline: names, specs, references, cycles, pipeline stages |
| `thorctl toolbox list` | List what a toolbox holds, with groups and versions |
| `thorctl toolbox convert` | Convert a legacy toolbox to format 2, or check that the converted file is current |
| `thorctl toolbox import` | Create the toolbox's items in a Thorium instance and publish its tool versions |
| `thorctl toolbox update` | Bring items that were imported earlier up to date; create nothing |
| `thorctl toolbox diff` | Show what an import would change, git-diff style |
| `thorctl toolbox export` | Read tools, pipelines, and optionally toolsets, agents, and workflows out of an instance into a toolbox |
| `thorctl toolbox remove` | Delete a previously imported toolbox's items |
| `thorctl toolbox init` | Scaffold a toolbox, tool, toolset, agent, workflow, or pipeline |
| `thorctl toolbox build` | Generate a `toolbox.json` from a directory of manifests |
| `thorctl toolbox build-images` | Build (and `--push`) the container images a toolbox builds |

The directory tree toolboxes are authored in, its manifests, how image tags are derived, `init`,
`build`, and `build-images` are covered in
[Toolbox Directories and Image Builds](./toolbox_directories.md). This page covers the manifest, and
reading, converting, importing, exporting, diffing, and removing. To change a single tool, agent, or
pipeline instead, see [Tools and Tool Instances](./tools.md) and [Editing
Resources](./editing_resources.md).

Every command that reads a toolbox takes a `PATH | URL | DIR`: a `toolbox.json` file, an `http(s)://`
URL, or a directory, which is searched for `toolbox.json` and then `toolbox.v2.json` (`convert` and
`build-images` prefer the legacy `toolbox.json`, since converting starts from it). Commands that
print a result take `--format` (`table`, `human`, `json`, or `yaml`), and `list` and `import` accept `-o` for it too.

#### Which command for which task

| You want to… | Use |
| ------------ | --- |
| Check a toolbox before sharing it, or in CI | `validate <toolbox>`, with `--strict` to fail on warnings |
| See what is in a toolbox | `list <toolbox>`, with `--kind` to narrow it |
| Stop converting a legacy toolbox on every read | `convert toolbox.json` once, commit the result, and check it with `convert --check` |
| Put a toolbox into an instance | `import <toolbox>`, after `import --dry-run` to see the plan |
| Import in CI without ever changing what exists | `import --skip-conflicts` |
| Publish changed tool specs as new versions | `import --overwrite` |
| Import only some items (and what they need) | `import -t NAME`, `-p NAME`, or `--only KIND/NAME` |
| Refresh only what you imported before | `update <toolbox>` |
| Gate CI on drift between a repo and an instance | `diff <toolbox> --exit-code` |
| Capture items from an instance | `export -g GROUP`, with `--all-kinds` for toolsets, agents, and workflows |
| Undo an import | `remove --dry-run`, then `remove` |
| Start a toolbox by hand | `init tool --kind ...`, then `build` ([directories](./toolbox_directories.md)) |
| Build the container images | `build-images --push` |
| Move to an air-gapped instance | `export --with-images`, push the images, then `import --image-path-prefix` |

### The format
---

A format 2 `toolbox.json` is a JSON object. Unknown top-level keys are refused, and every map is
sorted, so a file is byte-stable and can be compared and checked into version control.

```json
{
  "format_version": 2,
  "name": "My Toolbox",
  "registry": "ghcr.io/example/tools",
  "registries": ["ghcr.io/example/tools"],
  "groups": {"analysts": {"description": "Malware analysts"}},
  "network_policies": [],
  "tools": {},
  "builds": {},
  "toolsets": {},
  "agents": {},
  "workflows": {},
  "pipelines": {}
}
```

| Section | Holds |
| ------- | ----- |
| `groups` | The groups the items use, with descriptions. Advisory: an item may name a group that already exists without listing it here. Import creates a missing group only with `--create-groups`; `--group-override` replaces every item's groups with one |
| `network_policies` | Network policy definitions that tools name (created when missing; see [Network policies](#network-policies)) |
| `tools` | Tools by name, each with its versions (see below) |
| `builds` | Build-only contexts with no tool of their own, such as base images other builds start from (`builds.ghidra-base."12.0"`); `build-images` builds them first and import ignores them. In a toolbox directory each is a `type = "build"` manifest (see [Toolbox Directories](./toolbox_directories.md#tool-manifest-manifesttoml)) |
| `toolsets`, `agents`, `workflows` | The editable roots that agents and workflows use (see below) |
| `pipelines` | Pipelines, each version naming the tool behind every stage |

#### Tools and versions

```json
"tools": {
  "clamav": {
    "kind": "Image",
    "groups": ["static"],
    "description": "Antivirus scanner",
    "tags": {"category": ["antivirus"]},
    "latest": "latest",
    "versions": {
      "latest": {
        "dir": "images/github.com/Cisco-Talos/ClamAV",
        "spec": {"Image": {"environment": {"K8s": {"image": "ghcr.io/cisagov/thorium/tools/clamav:latest", "...": "..."}}}},
        "build": {
          "build_path": "images/github.com/Cisco-Talos/ClamAV",
          "build_image": true,
          "image_tags": ["ghcr.io/cisagov/thorium/tools/clamav:latest"]
        }
      }
    }
  }
}
```

- `kind` is `Image`, `Agentic`, or `Harness` and is derived from the spec when left out. Harness tools
  also take `harness_env` and `disabled_models`, and a Harness version takes `provides` (the tool
  versions it provides, as `{tool, version}`).
- Each version holds the **spec** inline (`spec`, the same document `thorctl tools spec` prints, with
  a top-level `Image`, `Agentic`, or `Harness` key) or as a path or URL (`spec_from`), plus an
  optional `build` section (the build context, tags, and base image `build-images` uses), `dir` (where
  its `manifest.toml` lives), and `network_policies_from`. A version that reuses another tool's or base
  build's image (`image_from` in its manifest, see [Reusing another tool's
  image](./toolbox_directories.md#tool-manifest-manifesttoml)) has that image in its spec and a `build`
  section with `build_image: false` and the image as its only tag, so it overrides only what its spec
  sets (entrypoint, args, resources) and is never built.
- `latest` names the label that should be the tool's latest version. A first import publishes
  versions oldest first (semantic labels ascending, then custom labels by name, then the `latest`
  label last), so the tool ends up pointing at the right one.
- Specs are checked as they are read: unknown keys are refused, as for `tools create`.

#### Spec hashes and version labels

Published versions are immutable, so a toolbox is compared with an instance by the **hash of each
version's spec** (the same hash Thorium stores, with references resolved to the target's ids), not by
its label. For each version in the manifest:

| Class | Meaning | Import does |
| ----- | ------- | ----------- |
| New | The tool doesn't exist in the group | Creates the tool with its first version |
| Unchanged | Some version of the tool already has this hash, whatever its label | Nothing |
| New version | The label is new for an existing tool | Publishes it (existing items are only changed with `--overwrite` or after you say Apply) |
| Label conflict | The label exists with a different spec | Publishes it as `LABEL+HASH` (the label plus the first 8 characters of the hash), since the label can't be reused, and says so |

Importing the same toolbox twice therefore changes nothing the second time. When a version depends on
something the same import creates, its hash isn't known until then, and it is classified again once
those dependencies exist.

#### References

Inside a spec, wherever the API wants a tool id, a toolbox writes a **ref** string that is resolved to
an id at import: `ref:tool/NAME` is the tool (following its latest version) and `ref:tool/NAME@LABEL` is
that version's instance. They are allowed in the slots that take ids: a tool's
`Image.data.dependencies.{results,children,filesystems}.tools` and an agentic tool's
`Agentic.backing.Container.tool_instance`. Elsewhere the manifest uses structured names:
`{"tool": NAME, "version": LABEL}` in `provides`, toolset `tools`, agent `tools` and `harness`, and
pipeline `stage_tools` (a missing or `null` version follows the tool's latest). A name that is in
neither the manifest nor the target group is an error naming the item (a warning in `validate`, when
the name may exist in the instance).

#### Toolsets, agents, and workflows

These are created and then updated in place; they are not versioned by the manifest (their instances
are built by Thorium when `auto_instance` is on).

```json
"toolsets": {"kit": {"groups": ["analysts"], "read_only": false, "selector": "category=triage",
                      "auto_instance": true, "tools": [{"tool": "strings"}, {"tool": "yara", "version": "1.0.0"}]}},
"agents": {"triage": {"groups": ["analysts"],
                       "config": {"system_prompt": "...", "max_turns": 20},
                       "model_auth": ["group"], "harness": {"tool": "pal-harness", "version": null},
                       "tools": [{"tool": "strings"}], "toolsets": [{"toolset": "kit"}], "sub_agents": ["summarizer"]}},
"workflows": {"flow": {"groups": ["analysts"], "agents": ["triage", "summarizer"], "entry_agent": "triage",
                        "limits": {"max_alive_sub_agents": 4}, "default_models": [{"model_id": "claude-sonnet"}],
                        "auto_instance": true}}
```

Agents are created first and their sub-agent edges wired once every agent exists, so agents that
delegate to each other in a cycle import. A workflow's `entry_agent` must be one of its `agents`. A
manifest never carries model endpoints, profiles, credentials, tokens, users, or secret values: a
spec's `env` values are readable by group members, so `validate` warns about names that look like
secrets (`warning:secret-env`) rather than hiding them.

#### Pipelines

```json
"pipelines": {
  "antivirus": {
    "versions": {
      "latest": {
        "dir": "pipelines/static/antivirus",
        "config": {"group": "static", "name": "antivirus", "order": [["clamav"]], "sla": 640800,
                    "triggers": {"RunOnNewSamples": "NewSample"}, "description": null},
        "stage_tools": {"clamav": {"tool": "clamav", "version": "latest"}}
      }
    }
  }
}
```

Every stage in `order` needs a `stage_tools` entry, which `validate` checks offline and `import` checks
before any write. `tool` names a tool in the manifest or already in the pipeline's group; `version` pins
that label (leave it out to follow the tool's latest). A pipeline name can be at most 25 characters.

### Validating and listing
---

`toolbox validate` reads a toolbox without talking to Thorium and reports every problem: unusable
names (including the names the API refuses for a new tool, toolset, workflow, or pipeline, such as `tool` or
`workflow`; see [Creating and publishing](./tools.md#creating-and-publishing)), specs that don't parse, unresolved references, dependency cycles, pipeline stages without a tool,
and more. Each line carries a code you can search for. It exits `1` when there are errors, or, with
`--strict`, when there are warnings too, and `0` otherwise. Real output for the repository's toolbox,
and for a toolbox with a mistake in it:

```text
$ thorctl toolbox validate tools/toolbox.json
Toolbox 'CISA Hosted Thorium Github Tools' (v2 format)
  tools: 59 (59 versions), builds: 1 (4 labels), toolsets: 0, agents: 0, workflows: 0, pipelines: 32, network policies: 1
Result: valid (0 errors, 0 warnings)

$ thorctl toolbox validate bad.json
Toolbox 'bad' (v2 format)
  tools: 1 (1 versions), builds: 0 (0 labels), toolsets: 0, agents: 0, workflows: 0, pipelines: 0, network policies: 0
error[missing_field:spec] tool x@1: has neither spec nor spec_from
Result: invalid (1 errors, 0 warnings)
```

| Severity | Effect |
| -------- | ------ |
| `error` | Import refuses. For example `missing_field:spec`, `invalid_value:*`, `missing_ref:tool` (a version of a tool the manifest defines that it doesn't have), `cycle:*` (a dependency cycle; agent-to-agent cycles are only a warning), and `missing_field:stage_tools.*` |
| `warning` | Fails `--strict`. For example `unresolved_ref:*` (a name that must already exist in the target group), `warning:secret-env`, `warning:uuid-literal` (an id written where a ref belongs), and `warning:conversion` |
| `note` | Information only, such as `note:undeclared-group` |

Names can't hold whitespace, control characters, or any of `/ @ : ,` and are at most 128 characters,
and labels can't hold whitespace or `/ @ ,`.

`toolbox list` prints what the toolbox holds (add `--kind tools|toolsets|agents|workflows|pipelines|builds`
to narrow it, and `-o json|yaml` for a document):

```text
$ thorctl toolbox list tools/toolbox.json
 KIND     | NAME                    | GROUPS | VERSIONS
----------+-------------------------+--------+-----------------------
 tool     | auto-volatility3        | static | latest
 tool     | auto-volatility3-worker | static | latest
 tool     | balbuzard               | static | latest
...
```

### Converting a legacy toolbox
---

`toolbox convert` reads a legacy toolbox (a path, URL, or directory) and writes the same toolbox as
format 2: each image becomes a tool whose spec is built exactly as the upgrade converts a stored image
(so an image in a toolbox and the same image in an instance hash alike), build-only entries become
`builds`, and each pipeline's stages name tools. Nothing is sent to Thorium and no login is needed.

```bash
thorctl toolbox convert old/toolbox.json                    # writes old/toolbox.v2.json
thorctl toolbox convert old/toolbox.json -o converted.json
thorctl toolbox convert old/toolbox.json -o -               # to stdout
thorctl toolbox convert old/toolbox.json --dry-run          # report and diff, write nothing
thorctl toolbox convert old/toolbox.json --check            # exit 1 if toolbox.v2.json is stale
thorctl toolbox convert old/toolbox.json --in-place         # replace it, keeping toolbox.json.bak
```

| Option | Description |
| ------ | ----------- |
| `-o, --output <PATH \| ->` | Where to write the result; the default is `<name>.v2.json` beside the input (`toolbox.json` becomes `toolbox.v2.json`), and a URL input needs `-o` unless `--dry-run` |
| `--in-place` | Write over the input after saving `INPUT.bak`; an existing `.bak` is an error unless `--no-backup` |
| `--check` | Write nothing; exit `1` with a diff when the output file differs from the conversion or is missing |
| `--dry-run` | Write nothing; print the report, and a diff against the output file when it exists |
| `--report <FILE \| ->` | Write the report as JSON to a file, or to stdout with `-`, instead of printing it on stderr |
| `--strict` | Write nothing and exit `1` when any item was skipped or any issue, warning, or collision was raised |
| `--group-override <GROUP>` | Put every tool and pipeline in this group |
| `--follow-latest` | Make pipeline stages follow their tool's latest version instead of pinning the version the legacy pipeline named |
| `--fill-image-from-tags` | Give an entry with no `image` the first of its build `image_tags` (default: skip it) |
| `--keep-incomplete-pipelines` | Keep a pipeline with a stage that has no converted tool, flagged `incomplete` (default: leave it out) |
| `-o/--format` | The report format on stderr: `table`, `human`, `json`, or `yaml` |

A toolbox that is already in format 2 converts to itself (`already in the v2 format; nothing to
convert`). The output is byte-stable (sorted maps, two-space JSON, a trailing newline), which is what
makes `--check` reliable. The conversion report goes to stderr, so `-o -` leaves stdout as the
manifest.

**Walkthrough: the repository's own toolbox.** `tools/toolbox.v1.json` in the Thorium repository is
the legacy toolbox the repository shipped before its image and pipeline manifests became tool and
stage-tool manifests. It holds 60 legacy image names (59 tools plus the four `ghidra-base` build
contexts) and 32 pipelines. Converting it, with real output:

```text
$ thorctl toolbox convert tools/toolbox.v1.json -o converted.json
Converted toolbox 'CISA Hosted Thorium Github Tools' (legacy format)
  59 tools (59 versions), 4 builds, 32 pipelines, 1 network policies
  pipeline antivirus: clamav -> clamav@latest
  pipeline auto-volatility3: auto-volatility3 -> auto-volatility3@latest
  ...
  pipeline yara: yara -> yara@latest
Result: 0 issues (0 skipped), 0 warnings
Wrote 'converted.json'
```

Each tool has `kind: Image`, its group, one version labelled `latest` with its `build` section, and the
four `ghidra-base` labels (`10.3.2`, `11.2`, `12.0`, `12.1`) are `builds`. Each of the 32 pipelines
names a tool for every stage, pinned to the `latest` label the legacy pipeline named, and the one
`allow-all` network policy appears once. The result is byte for byte the `tools/toolbox.json` the
repository's tool manifests built at the time they were rewritten (`toolbox build --rewrite-manifests`,
see [Toolbox Directories and Image Builds](./toolbox_directories.md)). `tools/toolbox.json` is now built
from those manifests and `tools/toolbox.v1.json` is not updated; it is kept for Thorctl releases older
than tool support, which read only the legacy format.

A directory that keeps a legacy `toolbox.json` next to its converted `toolbox.v2.json` can check the
pair with `thorctl toolbox convert toolbox.json --check -o toolbox.v2.json`. If someone edits one and
not the other, the check fails with a diff:

```text
$ thorctl toolbox convert toolbox.json --check -o toolbox.v2.json
...
--- toolbox.v2.json
+++ converted
@@ -1432,8 +1432,7 @@
             ]
           }
         }
-      },
-      "description": "edited by hand"
+      }
     },
...
'toolbox.v2.json' differs from the converted toolbox
```

and exits `1`. Regenerate with `toolbox convert toolbox.json -o toolbox.v2.json` and commit both
files.

**What conversion skips or changes.** Anything the conversion can't carry is listed with a code, and
`--strict` turns any of them into a failure. A tool that is left out is never half-converted:

```text
issue[missing_field:image] image yara@latest: names no container image, so it was not converted into a tool
issue[invalid_value:stages.*] pipeline yara@latest: runs a stage whose tool was not converted
Result: 2 issues (2 skipped), 0 warnings
```

| Code | Meaning | Effect |
| ---- | ------- | ------ |
| `missing_field:image` | A K8s or Windows entry with no container image | The tool is skipped (`--fill-image-from-tags` uses its first build tag) |
| `invalid_value:stages.*` | A pipeline stage with no converted tool, in another group, or pinning a missing label | The pipeline is left out (`--keep-incomplete-pipelines` keeps it, flagged) |
| `invalid_value:name`, `invalid_value:version` | A name or label that can't be a tool name or version | The entry is skipped |
| `dropped_dependency:results\|children\|filesystems` | A dependency image name that matches no converted tool | The name is removed and the tool kept; if that empties the list the tool is skipped |
| `collision:NAME` | One image name in different groups | The conversion fails unless `--group-override` is given |
| `warning:secret-env` | An env variable whose name looks like a secret | Warning only |

### Importing
---

```text
thorctl toolbox import <PATH | URL | DIR> [OPTIONS]
```

`import` creates the toolbox's items in your instance in dependency order and publishes each tool's
versions. It first reads the toolbox, converts a legacy one in memory, validates it, and applies any
selection, all **before it logs in or sends a request**, so a bad toolbox or `--strict` is refused
without touching the instance. It then plans from the instance's current state, applies the plan, and
prints a report. The plan is the dry-run output.

```bash
thorctl toolbox import ./my-toolbox --dry-run                 # the plan; nothing is written
thorctl toolbox import ./my-toolbox                           # asks per differing item at a terminal
thorctl toolbox import tools/toolbox.json --skip-conflicts    # create what is missing, change nothing
thorctl toolbox import https://example.com/toolbox.json --overwrite --create-groups
thorctl toolbox import ./my-toolbox -p antivirus              # one pipeline and the tools it needs
thorctl toolbox import ./my-toolbox --only workflow/flow --only tool/strings
thorctl toolbox import ./my-toolbox --group-override sandbox --yes
```

| Option | Description |
| ------ | ----------- |
| `--dry-run` | Show what would be created, published, and updated, and write nothing |
| `--overwrite` | Apply every difference without asking |
| `--skip-conflicts` | Create what is missing but never change what already exists; each differing item is listed |
| `--skip-confirm` (alias `--yes`) | Skip the "go ahead?" question before writing |
| `-t, --tools <NAME>` | Only these tools (comma separated) and what they need |
| `-p, --pipelines <NAME>` | Only these pipelines and the tools they need |
| `--only <KIND/NAME>` | Only this item and what it needs; `KIND` is `tool`, `toolset`, `agent`, `workflow`, or `pipeline`; repeatable |
| `--no-pipelines` | Don't import pipelines |
| `--group-override <GROUP>` | Put every item in this group (created if missing) |
| `--create-groups` | Create the groups the toolbox names that don't exist, owned by you |
| `--strict` | Stop before any write if converting a legacy toolbox raised an issue or the manifest has a warning (other than `unresolved_ref:*`, since the plan checks those names against the instance) |
| `--follow-latest` | Make a legacy toolbox's pipeline stages follow their tool's latest version |
| `--rollback-on-failure` | Roll back what was applied if the import stops early |
| `--update-network-policy` | Make existing network policies match the toolbox (see [Network policies](#network-policies)) |
| `--image-path-prefix <REGISTRY/BASE>` | For a bundled toolbox, rewrite each tool's image to `<prefix>/<group>/<name>:<tag>` (see [Offline transfer](#offline-transfer-with-bundled-images)) |
| `-o, --format <FORMAT>` | `table`, `human`, `json`, or `yaml` |
| `--editor <EDITOR>` | Accepted for compatibility and ignored; differences are decided with Apply, Skip, or Quit |

**Selection.** `-t`, `-p`, and `--only` select roots together with everything they depend on in the
manifest (a pipeline's tools, a tool's dependency tools, a workflow's agents and their tools). A
network policy is kept only when a kept tool's spec names it.

**What an import does to items that exist.** An item that already exists and would change is a
*conflict*: a tool with a new version or label conflict or a different root, or a toolset, agent,
workflow, or pipeline whose normalized definition differs. How conflicts are decided depends on the
flags and the session:

- `--skip-conflicts` leaves **every** existing item alone, including new versions of existing tools, and
  lists them.
- `--overwrite` applies all of them: it publishes new versions (a label conflict as `LABEL+HASH`) and
  updates toolsets, agents, workflows, and pipelines in place.
- With neither flag and a terminal, each differing item asks **Apply**, **Skip**, or **Quit**, then
  `Go ahead?` before anything is written (`--skip-confirm` skips that question).
- With neither flag and no terminal, the import refuses before any write and lists the conflicts.

For an existing tool, import adds what the manifest names and removes nothing: it sets the description
when the manifest has one and adds missing groups and tag values. A harness tool's `harness_env` and
`disabled_models` follow the manifest when it sets them. Toolsets, agents, workflows, and pipelines are
authoritative, so their groups and edges are changed to match (a manifest `description: null` clears a
description on those).

**Rights.** Import checks before any write that you can write to every group something would be written
to: an admin, or an owner, manager, analyst, or user with the `Entities` allow flag. A group that doesn't
exist needs `--create-groups` (or `--group-override`); a group you can't write to is an error only when
something would be written there. A non-admin is also refused, before any write, for a tool whose spec
has a non-default `security_context`, because the API refuses it. The repository's toolbox has tools
that run as root, so **importing it needs an admin login**, which is why the Helm chart's import Job
logs in as the bootstrap admin (see [Importing a toolbox](../admins/deploy/deploy-helm.md#importing-a-toolbox)).
A pipeline name over 25 characters is refused as well.

**Failure and rollback.** The first failure stops new work: items of the wave already running finish,
and everything not reached is reported `not_attempted`. Every write is journaled. With
`--rollback-on-failure`, or when a terminal answers yes to the question, the journal is replayed newest
first (pipelines, workflows, agents, toolsets, tool versions then the tool, policies, groups last), and
in-place updates are restored to what they were; failures while rolling back are listed and aren't
fatal. Without either, what was applied is kept and reported. Re-running the import after fixing the
cause only applies what is still missing.

**Output.** A dry run and a refusal print the plan document (`{plan, errors}`) on stdout and a refusal
also prints each error on stderr; any other run prints the report (`outcome`, `plan`, `groups_created`,
`policies_created`, `policies_updated`, `results`, `rolled_back`, `rollback_errors`, `errors`, and `notes`).
The legacy conversion report goes to stderr as `convert` prints it.

| Exit code | Meaning |
| --------- | ------- |
| `0` | Applied, nothing to do, a dry run, or you quit or cancelled |
| `1` | The import failed, was rolled back, or was refused (including `--strict`, errors in the manifest, and conflicts with no way to ask) |
| `2` | The flags were bad |

### Updating what you imported
---

`toolbox update` takes the same flags as `import` (except `--create-groups`) but only touches tools,
toolsets, agents, workflows, and pipelines that **already exist** in the target groups: nothing is
created, no group is created, and the items that are missing are listed as not imported. Use it to
refresh an instance from a newer copy of a toolbox:

```bash
thorctl toolbox update ./my-toolbox --dry-run
thorctl toolbox update ./my-toolbox --overwrite
```

### Exporting from a running instance
---

`toolbox export` reads tools (every version's spec), pipelines, and with `--all-kinds` toolsets, agents,
and workflows out of an instance and writes a toolbox directory: one folder and `manifest.toml` per
item (see [Toolbox Directories and Image Builds](./toolbox_directories.md#toolbox-layout)) and a format 2
`toolbox.json` with every spec inline, ready to validate, diff, and import elsewhere. Importing the
export into an empty group reproduces the same spec hashes. Container images stay in their registry;
model endpoints, profiles, and credentials are never exported.

```bash
# every tool and pipeline of a group into a new toolbox
thorctl toolbox export -g static -o ./my-toolbox
# also its toolsets, agents, and workflows
thorctl toolbox export -g analysts --all-kinds -o ./my-toolbox
# specific items; what they reference is included automatically
thorctl toolbox export -g analysts --pipelines triage --tools strings --workflows malware-triage
# only each tool's latest version, and keep old pinned ones
thorctl toolbox export -g static --latest-only -o ./my-toolbox
# append into an existing toolbox by pointing at its config.toml
thorctl toolbox export -c ./my-toolbox/config.toml --tools static/yara
# refresh every item already in the toolbox from the instance
thorctl toolbox export -c ./my-toolbox/config.toml --overwrite
```

| Option | Description |
| ------ | ----------- |
| `-g, --group <GROUP>` | Export everything of the selected kinds from this group; with `-t`, `-p`, `--toolsets`, `--agents`, or `--workflows` it is only the default group for names given without one |
| `-t, --tools`, `-p, --pipelines`, `--toolsets`, `--agents`, `--workflows` `<GROUP/NAME[=DIR]>` | Export specific items (comma separated). `=DIR` places the item's files in `DIR` under the toolbox root (placement only). `-i/--images` is a hidden alias of `-t` |
| `--all-kinds` | With `-g` and no named items, also export toolsets, agents, and workflows |
| `--include-deprecated`, `--latest-only` | Include deprecated versions (skipped by default) or keep only each tool's latest; a version something pins is always exported |
| `--group-override <GROUP>` | Write every item under this group instead of its source group |
| `-o, --output <DIR>` | The toolbox root (default: the `--config` directory, else `./toolbox`) |
| `-c, --config <CONFIG.TOML>`, `--name`, `--registry` | Where the toolbox's name and registry come from (an existing `<output>/config.toml` wins and is kept) |
| `--skip-conflicts`, `--overwrite`, `--overwrite-config` | What to do with items already in the output (below) |
| `--review` | Open each item in an editor to tweak before it is written (needs a terminal; `--editor` picks the editor) |
| `--with-images` | Pull and save each K8s tool's container into the toolbox as `<dir>/<name>.tar.gz` |
| `--strip-registry` | Write each tool's container url empty so a rebuild derives it from the toolbox's registry. Needs a registry, conflicts with `--with-images`, and hashes differently from the tool in Thorium |
| `--format <FORMAT>` | The format of the printed result |

**Selection and references.** With `-g` alone you get tools and pipelines; naming any item selects only
what you named. Anything a selected item references is included automatically: the tools of a
pipeline, toolset, agent, workflow, spec slot, or harness `provides`, the toolsets and agents an agent or
workflow names, and the instance a pin points at, even when deprecated or not the latest. An
auto-included item from another group keeps its own group. A tool with no versions left after filtering
keeps its newest. Pipelines with a stage that has no stage tool (from before the migration) are skipped
with a warning, because the API would refuse them on import. Two different items of one kind with one
name in different groups refuse the export; export one at a time.

**Existing output.** Appending works per item: an identical item is left alone (missing files are
written), a differing one is replaced with `--overwrite`, left with `--skip-conflicts`, and with neither
flag the export stops before any write, whether or not there is a terminal. `--overwrite` with no
selection and no `-g` refreshes every item already in the output. A legacy `toolbox.json` in the output
directory is refused. `config.toml` is kept when present, else seeded from `-c`, else built from
`--name` and `--registry`, and written with `format_version = 2`.

### Diffing a toolbox against an instance
---

`toolbox diff` shows what an import would change. Each tool version is listed as new, a new version,
or a label conflict (unchanged versions are only counted), toolsets, agents, workflows, and pipelines
that differ show unified hunks of their normalized definitions, and a summary counts each kind.

```bash
thorctl toolbox diff ./my-toolbox
# gate CI on drift: exit 1 when an import would change anything
thorctl toolbox diff ./my-toolbox --exit-code
thorctl toolbox diff ./my-toolbox --group-override sandbox --format json
```

A tool version counts as unchanged when any version of the live tool has the same spec hash, the same
rule import uses. `--exit-code` exits `1` when the plan would write anything (new tools and versions,
label conflicts, new or differing toolsets, agents, workflows, and pipelines, or differing network
policies); items only in the instance never count. Errors exit `1` as well, so exit code `1` alone doesn't
distinguish drift from a failed diff. `--format json` prints `{changed, summary, versions, plan, notes}`.
Tool roots (description, tags, groups) aren't compared. `--image-path-prefix` compares a bundled
toolbox's image urls as an import with that prefix would store them.

### Network policies
---

A toolbox carries network policy *definitions* so the references in its specs stay valid on import:

- `toolbox export` records each policy a K8s spec names in the manifest's `network_policies` (a policy
  it can't read is a warning) and each exported policy's `groups` are scoped to the groups of the items
  that name them. Each exported tool version's folder also gets a `<policy>.policy.json` file for every
  exported policy its spec names, listed in its manifest's `network_policies_from`, so building the
  exported directory carries the policies too.
- `toolbox build` bundles `network_policies_from` files into `toolbox.json`; URL entries are fetched at
  import time. A toolbox with two different definitions under one policy name fails validation.
- `toolbox import` creates missing policies before any tool, in the toolbox's groups (or the
  `--group-override` group). A policy that exists and differs is left alone with a warning naming the
  differing fields, unless you pass `--update-network-policy`: then the toolbox's definition wins (rules
  and flags are overwritten, missing groups added, groups never removed). Creating a policy needs an
  admin, and that update is applied **without a prompt** when the session can't ask. Created and updated
  policies are part of the rollback.

### Removing a toolbox
---

`remove` deletes the items a toolbox names from the instance, in dependency order: pipelines,
workflows, agents, and toolsets first, then each tool's versions and the tool. Groups and network
policies are never deleted.

```bash
thorctl toolbox remove ./my-toolbox --dry-run            # no terminal needed; exits 0
thorctl toolbox remove ./my-toolbox                      # asks before deleting
thorctl toolbox remove ./my-toolbox --group-override sandbox --yes
```

- A legacy toolbox is converted first, so its images are removed as the tools an import created.
- The plan is printed before anything is deleted. Items that don't exist are reported and skipped.
- A tool is **left whole** when a pipeline outside the toolbox runs it or one of its versions, or when
  a tool the toolbox leaves in place depends on it. Versions the toolbox names (and the `LABEL+HASH`
  versions published beside them) are deleted, and a tool with other versions stays after those go.
- Toolsets, agents, and workflows outside the toolbox that reference a tool aren't checked in advance;
  the API's `409` is reported and that tool's remaining steps are skipped.
- Anything left, refused, or failed makes the command exit `1` (`not fully removed`).
- Without a terminal the command fails naming `--skip-confirm`/`--yes` unless you pass `--dry-run`.
  `--group-override` is for a toolbox that was imported with `--group-override`.

Single items are deleted with `thorctl tools delete`, `toolsets delete`, `agents delete`, `workflows
delete`, and `pipelines delete`.

### Offline transfer with bundled images
---

To move a toolbox, including its container images, to an air-gapped instance, bundle the images on a
connected host and push them to the offline registry yourself; import **does not push containers**.

1. **On a connected host**, export with `--with-images`. Each K8s tool's container is pulled and saved
   with the configured container runtime (docker or podman) to `<dir>/<name>.tar.gz`, and the toolbox is
   marked as bundled. Tools with other scalers don't run from a container and are exported without a
   tarball. Bundling is best effort: an image that can't be pulled or saved is warned about and the rest
   is written, but the export then exits non-zero as incomplete.

   ```bash
   thorctl toolbox export -g static --with-images -o ./offline-toolbox
   ```

2. **Move** the directory to the offline environment and **push the images** to the offline registry
   (for example `docker load`, retag, and `docker push`) under `<prefix>/<group>/<name>:<tag>`.
3. **Import** with `--image-path-prefix`, which rewrites each bundled K8s tool's image to that path so
   the tools point at the registry you pushed to:

   ```bash
   thorctl toolbox import ./offline-toolbox --image-path-prefix registry.offline.local:5000/team
   # clamav (group "static") -> registry.offline.local:5000/team/static/clamav:latest
   ```

   Thorctl warns that images are not pushed. With `--group-override`, the images are expected under the
   override group. `toolbox diff --image-path-prefix ...` previews the rewritten urls. Without a prefix
   the images keep the urls the manifest names.

A KVM tool round-trips its `kvm` settings, but the VM disk image is not part of any export.

### In a cluster: the toolbox import Job
---

The Helm chart can import a toolbox once Thorium is up, with the same command described above. See
[Importing a toolbox](../admins/deploy/deploy-helm.md#importing-a-toolbox) for the values and how the Job
runs, and [Development Clusters (minithor)](../admins/deploy/minithor.md) for the `--toolbox` option.

### Upgrading
---

Toolbox commands changed in several ways that can affect existing scripts: `toolbox import` works with
tools again (it refused to run in the first agentic release), reads both formats, and has new flags and
a report; `export`, `diff`, `remove`, and `build-images` read and write tools; and `validate`, `list`,
`convert`, and `update` are new. See
[Upgrading: Changes to Tool Commands](./tool_cli_changes.md) for the full list.
