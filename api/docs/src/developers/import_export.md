# Importing, Exporting, and Editing Images and Pipelines

Thorctl can copy image and pipeline configurations out of one Thorium instance, move them (along
with their container images) to another instance, and edit them in place. This page covers:

| Command | Purpose |
| ------- | ------- |
| `thorctl images export` / `thorctl pipelines export` | Write configs (and container image tarballs) from a group to a local directory |
| `thorctl images import` / `thorctl pipelines import` | Create or update images/pipelines in a group from an export directory |
| `thorctl images edit` / `thorctl pipelines edit` | Edit one existing image or pipeline in your text editor |
| `thorctl images delete` / `thorctl pipelines delete` | Delete images or pipelines from a group |

> **Images are read-only in this release.** Thorium now runs tools instead of images: creating or
> updating an image returns `410 Gone`, while images can still be listed, described, exported, and
> deleted. `thorctl images import` and `thorctl images edit` probe for this first and stop before
> they open an editor or push a container, printing `Images can no longer be created or updated;
> use tools (thorctl tools create|edit|publish)`. Add tools with `thorctl tools create` (see
> [Tools and Tool Instances](./tools.md)) or import them with a [toolbox](./toolbox.md), which also
> converts a legacy image toolbox to tools.
>
> **Pipelines carry their tools instead of images.** `thorctl pipelines export` writes each stage's
> tool by name and version in `stage_tools` and no longer exports the stages' images or their container
> tarballs, and `thorctl pipelines import` looks those tools up by name in the target group, creates
> or updates only pipelines, and fails before any request when a stage has no tool (an export from an
> older Thorctl has none). The image options of `pipelines export` and `pipelines import`
> (`--config-only`, `--registry`, `--registry-override`, `--skip-push`, `--migrate-registry`) are still
> accepted but have no effect. `pipelines edit` changes the pipeline and its stage tools through the
> shared edit flow described in [Editing Resources](./editing_resources.md), which confirms the
> changes after an editor session.

> The image and pipeline sections below describe the image-era engine; where they mention images
> being created, pushed, or bundled with pipelines, read them with the notes above in mind.
>
> `images import`/`export` and `pipelines import`/`export` are available in the Linux and macOS
> builds of Thorctl. `edit` and `delete` are available on every platform.

### Import/export or a toolbox?
---

Both move tools between instances, but they fit different jobs:

- Use **`pipelines` export and import** for a quick, one-off copy of a group's pipelines to another
  instance (or back into the same one), and **`images export`** to read an image's config. The export directory is a flat
  snapshot, and you choose the target group when you import. These commands give you fine-grained
  control over registries (`--registry`, `--registry-override`, `--migrate-registry`,
  `--skip-push`).
- Use a **[toolbox](./toolbox.md)** when the tools should live as a versioned, reviewable source
  tree: many images and pipelines packaged as one unit with manifests, build contexts, docs, and
  network policy definitions. A toolbox can be published by URL, diffed against an instance
  (`toolbox diff`), removed as a unit (`toolbox remove`), and bundled for air-gapped transfer.

A toolbox import has its own engine, described in [Toolboxes](./toolbox.md): it plans from spec
hashes and applies in dependency order, with its own conflict flags, per-item prompt, and rollback.
The pipeline conflict modes, interactive merge, and change detection described on this page apply
to `pipelines import`.

### Naming images and pipelines
---

Commands that take a single image or pipeline *target* accept three forms:

| Form | Example | Notes |
| ---- | ------- | ----- |
| `GROUP/NAME` | `static/scan` | Preferred |
| `NAME:GROUP` | `scan:static` | Also accepted |
| `NAME` | `scan` | Thorctl searches your groups; fails if the name exists in more than one group |

These forms are accepted by `thorctl images describe`, `thorctl pipelines describe` (both on
the command line and in `-L/--list` files), `thorctl reactions create -p`, and
`thorctl files upload -p`. `files upload -p` requires the group (`GROUP/NAME` or `NAME:GROUP`)
because it doesn't search for it. A target that mixes `/` and `:` (`static/scan:x`), has an empty
part (`/scan`, `scan:`, `:static`), or has more than one separator is rejected:

```bash
thorctl images describe static/clamav
thorctl pipelines describe scan:static
thorctl reactions create -p static/scan <SHA256>
thorctl files upload -g static -p static/scan ./samples
```

`import`, `export`, and `delete` take plain names plus a `-g/--group` flag, and `edit` takes the
group as an optional second positional argument (`thorctl images edit clamav static`).

Thorctl's output names resources the same way:

| What | Notation | Example |
| ---- | -------- | ------- |
| An image or pipeline in Thorium | `group/name` | `Deleted image 'static/clamav'` |
| A toolbox manifest entry | `group/name@version` (or `name@version` when the group isn't known yet) | `Unchanged: image 'static/clamav@1.2' already current in the toolbox` |
| A network policy | `name (group: g, id: X)`, `name (id: X)`, or `name (groups: a, b)` | `Created network policy 'allow-dns' (group: static, id: 5f0c…)` |

When a bare name is ambiguous, the error lists the candidates as `group/name` so you can copy the
one you meant.

### The export directory
---

Export writes one JSON config per resource. Import reads the same layout, so pass the directory
root (the one containing `images/` and `pipelines/`) to `-i/--import`:

```text
exports/
├── images/
│   ├── clamav.json        # image config
│   ├── clamav.tar.gz      # container image (K8s images only; omitted with --config-only)
│   └── yara.json
└── pipelines/
    └── scan.json          # pipeline config
```

A config's file name is the resource's name: `images/clamav.json` must contain
`"name": "clamav"`, or the import fails before changing anything. The `group` field in a config is
ignored on import; everything lands in the group given with `-g/--group`.

#### Config field order

Every config Thorctl writes or shows (both exports, `toolbox export`, `toolbox init`, `edit`, the
import merge editor, and `toolbox diff`) uses the same top-level key order, which matches the
order of the Web UI's edit forms:

- **Images:** `name`, `group`, `creator`, `bans`, `description`, `version`, `scaler`, `image`,
  `timeout`, `lifetime`, `runtime`, `display_type`, `spawn_limit`, `collect_logs`, `generator`,
  `resources`, `args`, `output_collection`, `dependencies`, `env`, `volumes`, `network_policies`,
  `child_filters`, `modifiers`, `clean_up`, `kvm`, `security_context`
- **Pipelines:** `name`, `group`, `description`, `sla`, `order`, `triggers` (an exported
  pipeline also holds `stage_tools`, which is written after them with the other keys)

Keys not in these lists are written at the end in alphabetical order. Arrays that are really sets
(`network_policies` and the `child_filters` `mime`, `file_name`, and `file_extension` lists) are
written sorted, so exporting the same resource twice produces byte-identical files.

## Exporting
---

```text
thorctl images export    -g <GROUP> [-o <DIR>] [OPTIONS] [IMAGE]...
thorctl pipelines export -g <GROUP> [-o <DIR>] [OPTIONS] [PIPELINE]...
```

With no names, every image (or pipeline) in the group is exported; exporting an empty group is an
error. `pipelines export` writes each pipeline's `stage_tools` as `{tool, version}` objects (names
and version labels), so the directory can be imported into any group that has tools of those names.
It doesn't export the stages' images.

| Option | Short | Default | Description |
| ------ | ----- | ------- | ----------- |
| `IMAGE...` / `PIPELINE...` | | every resource in the group | The names to export |
| `--group <GROUP>` | `-g` | required | The group to export from |
| `--output <DIR>` | `-o` | `exports` | The directory to export into |
| `--config-only` | | off | Write configs only, without container image tarballs |
| `--overwrite` | | off | Overwrite on-disk configs that differ without prompting. Conflicts with `--skip-conflicts` |
| `--skip-conflicts` | | off | Write new configs but leave differing on-disk configs untouched, with a warning for each and a one-line summary of how many were skipped |
| `--review` | | off | Open each config in your editor before it's written (needs a terminal) |
| `--editor <EDITOR>` | | see [Choosing an editor](#choosing-an-editor) | The editor for `--review` and for merging on-disk conflicts |

The global `--workers` flag (default `10`) limits how many resources are fetched and how many
tarballs are saved at once. The global `--container-runtime` flag chooses between docker and podman
(see [Container runtime](#container-runtime)). The global `-q/--quiet` flag hides progress bars and
informational lines; warnings, errors, and the final line still print.

**Tarballs.** Unless `--config-only` is set, each **K8s** image that has a container url is pulled
and saved to `images/<name>.tar.gz`. Other scalers (`BareMetal`, `Windows`, `Kvm`, `External`)
are exported config-only, since import never loads a container for them. A tarball is saved again
whenever its config is written; when the config on disk is left as-is, the tarball is only saved
if it's missing, so a large archive isn't re-pulled on every run.

### Examples

Export every image in the `static` group, then every pipeline:

```bash
thorctl images export -g static -o ./static-export
thorctl pipelines export -g static -o ./static-export
```

Export one pipeline (for example to review it or to import it into an instance that has tools of
the same names):

```bash
thorctl pipelines export -g static scan -o ./scan-export
```

Tweak each config before it's written, using VS Code:

```bash
thorctl images export -g static clamav --review --editor "code --wait"
```

`--review` opens each config as YAML (in the order above). When you save and close the editor, the
result is checked against the image/pipeline schema and written back as JSON. If it doesn't parse,
you're offered **Edit** (reopen the editor) or **Cancel**; Cancel writes the config as it was
exported. Renaming a resource in review isn't supported and fails that resource. `--review` needs
a terminal on stdin, stdout, and stderr and fails up front otherwise.

### Re-exporting over an existing directory

A config whose on-disk content is byte-identical to what would be written is left alone. When an
existing file differs, Thorctl asks what to do:

```text
Conflict: 'exports/images/clamav.json' already exists on disk with different content.
> Merge         - open an editor to merge the on-disk and new versions
  Overwrite     - replace the file on disk
  Skip          - keep the file on disk unchanged
  Overwrite all - replace this and every later conflict
  Skip all      - keep this and every later conflict
  Quit          - stop the export
```

- **Merge** opens a git-style conflict view (`<<<<<<< Existing (on disk)` /
  `>>>>>>> New (from export)`). The file is saved only once every marker is resolved and the result
  parses. `.json` files must be strict JSON (no comments, unquoted keys, or trailing commas),
  because import reads them as JSON. Cancelling the merge keeps the on-disk file.
- **Overwrite all** and **Skip all** apply to the rest of the run.
- **Quit** stops writing configs. `images export` still saves tarballs for the configs already
  written. The export then ends with `Export stopped early` and exits non-zero.

How conflicts are handled depends on the flags and the session:

| Situation | Differing on-disk file |
| --------- | ---------------------- |
| Terminal on stdin and stderr, no flags | Prompt (above) |
| `--overwrite` | Overwritten, no prompt |
| `--skip-conflicts` | Left untouched, with a warning for each and a summary line |
| No terminal (CI, cron, `2>&1 \| tee`, …) and no flags | Left untouched, and the export ends in an error naming `--skip-conflicts` and `--overwrite` |

Without a terminal and without a flag, the export still writes the new configs, but it can't ask what
to do about the differing ones, so it leaves them as they are and exits `1`: a script can tell that
something was not written. Pass `--skip-conflicts` to keep the differing files on purpose (the export
then prints `Skipped N existing config(s) that differ (...); --skip-conflicts left them untouched`,
also with `-q`, and exits `0`) or `--overwrite` to replace them. The summary names at most ten files
and counts the rest.

> **Files from an older Thorctl.** Configs written by older Thorctl versions use a different key
> order, so the first re-export reports each one as a conflict even when nothing has changed in
> Thorium. Choose **Merge** to check for real changes, or **Overwrite all** (or `--overwrite`) if
> you have no local edits to keep. Later re-exports are byte-stable.

### Exit status

| Result | Final line | Exit code |
| ------ | ---------- | --------- |
| Everything written | `Export complete!` | 0 |
| A config or tarball failed (fetch error, pull/save failure, …) | `Export finished with errors`, then e.g. `Failed to export 1 image(s): yara; 1 image tarball(s): clamav` | 1 |
| Quit at a conflict prompt | `Export stopped early` | 1 |
| Differing on-disk configs, no terminal, and neither `--skip-conflicts` nor `--overwrite` | `Export finished with errors`, then `N existing config(s) on disk differ from the export and no terminal is available to ask; pass --skip-conflicts to keep them or --overwrite to replace them: ...` | 1 |
| Empty group | `No images found in group 'static'` | 1 |

## Importing
---

```text
thorctl images import    -g <GROUP> -i <DIR> [OPTIONS] [IMAGE]...
thorctl pipelines import -g <GROUP> -i <DIR> [OPTIONS] [PIPELINE]...
```

With no names, every config in `<DIR>/images` (or `<DIR>/pipelines`) is imported.
`pipelines import` imports only pipelines, under one rollback journal. Each stage's tool is looked up
by name in the target group (a stage config may name it `{tool, version}`, as `"NAME[@VERSION]"`, or
by id), and a stage with no tool, or a tool the group doesn't have, fails the import before anything
changes. `images import` no longer works (see the note at the top of this page).

| Option | Short | Default | Description |
| ------ | ----- | ------- | ----------- |
| `IMAGE...` / `PIPELINE...` | | every config in the directory | The names to import |
| `--group <GROUP>` | `-g` | required | The group to import into (created if it doesn't exist) |
| `--import <DIR>` | `-i` | required | The export directory to read |
| `--registry <REGISTRY>` | `-r` | | Retag pushed containers onto this registry (see [Registries](#registries-and-containers)) |
| `--registry-override <URL>` | | | Rewrite the registry of every image's stored url |
| `--skip-push` | | off | Don't load, retag, or push container tarballs; configs are still imported |
| `--migrate-registry` | | off | Only update the stored url of existing images; create missing images (and, for `pipelines import`, missing pipelines). Conflicts with `--overwrite` and `--skip-conflicts` |
| `--overwrite` | | off | Apply every incoming change to existing resources without prompting. Conflicts with `--skip-conflicts` |
| `--skip-conflicts` | | off | Never update existing resources; warn about each one that differs and print a one-line summary of how many were skipped |
| `--rollback-on-failure` | | off | Undo applied changes automatically if the import stops early in a session that can't prompt |
| `--editor <EDITOR>` | | see [Choosing an editor](#choosing-an-editor) | The editor for the interactive merge |

The global `--workers` flag limits how many resources are created or updated at once, and
`--container-runtime` chooses docker or podman.

### What an import does

1. **Load and check everything first.** Every config is read and parsed. A config whose `name`
   doesn't match its file name, a pipeline with an empty `order` or an empty stage, or (for
   `pipelines import`) a stage with no tool or a tool that isn't in the group stops the import before
   anything changes.
2. **Compare with Thorium.** Each resource is labeled *new*, *changed*, or *unchanged* (see
   [What counts as a change](#what-counts-as-a-change)).
3. **Confirm.** In an interactive session (the default mode with a terminal on stdin and stderr),
   you're shown a summary and asked to confirm **if** an existing resource would change or the
   group must be created. Otherwise nothing is asked; a missing group is announced as
   `Group 'static' does not exist and will be created`.
4. **Create the group** if it's missing.
5. **Apply images, then pipelines.** New resources are created. Existing ones are handled by the
   conflict mode. Containers are loaded and pushed only for images that are actually created or
   updated. Unchanged and skipped images are never re-pushed.
6. **Settle.** If the import stopped early, you're offered a rollback (see [Rollback](#rollback)).
   Then the final line and exit code are set (see [Exit status](#exit-status-1)).

A confirmation looks like this:

```text
New Images:
  static/yara
Existing Images (will prompt for action):
  static/clamav [changed]
New Pipelines:
  static/scan
New Groups:
  static

Import the above items to Thorium instance at 'https://thorium.example.com' as user 'alice'? [y/n]
```

Answering `n` prints `Import cancelled; nothing was changed` and exits 0.

**Group creation.** Thorctl compares the target group against the groups you can see. Admins see
every group; other users see only groups they belong to. If the group exists but you aren't a
member, it looks missing, so Thorctl tries to create it and the import fails with
`Failed to create group 'static': …` before any image or pipeline is touched. Ask a group owner to
add you, then import again.

### Conflict modes

New resources are created in every mode. Existing resources that differ are handled like this:

| Mode | How to select it | Changed existing resources |
| ---- | ---------------- | -------------------------- |
| Interactive | default, with a terminal on stdin **and** stderr | Prompted one at a time: Edit, Skip, Apply, or Quit |
| Interactive without a terminal | default, when stdin or stderr isn't a terminal | The import refuses before changing anything and exits `1`, naming `--skip-conflicts` and `--overwrite` (when no existing resource differs, it goes on) |
| Overwrite | `--overwrite` | Every incoming change is applied; no prompts |
| Skip conflicts | `--skip-conflicts` | Never touched; a warning lists the differing fields, and a summary line counts them |

The skip warning names each field that differs, so the skip can be audited, and a summary line
follows the import (it prints even with `-q`):

```text
Warning: images import - Skipping image 'static/clamav' (differs: [args, timeout]); re-run with --overwrite or resolve interactively
Warning: images import - Skipped 1 existing resource(s) that differ (image static/clamav); --skip-conflicts left them untouched
```

With no terminal and neither flag, an import that finds differing resources has no one to ask, so it stops
before creating anything and exits `1`, the same way `toolbox import` and the delete commands refuse:

```text
Error: 1 existing resource(s) differ from the import and no terminal is available to ask; pass --skip-conflicts to leave them untouched or --overwrite to apply the differences: image static/clamav
```

The summary names at most ten resources and counts the rest. `--migrate-registry` never asks about
conflicts, so it is not refused.

Only the interactive mode with a terminal ever prompts. That covers the confirmation, the merge
prompt, and the rollback question. With `--overwrite` or `--skip-conflicts`, nothing prompts even
on a terminal. Note that `2>&1 | tee log.txt` makes stderr a pipe, so the run is non-interactive.

### The interactive merge

For each existing resource that would change, Thorctl asks:

```text
Image 'static/clamav' has changes:
> Edit   - Open editor to review and resolve conflicts
  Skip   - Keep the existing configuration unchanged
  Apply  - Accept all incoming changes
  Quit   - Stop processing remaining resources
```

- **Edit** opens both versions in your editor as one YAML document, with git-style markers around
  each difference:

  ```yaml
  '*name*': clamav
  '*group*': static
  ...
  scaler: K8s
  image: ghcr.io/cisagov/clamav:1.2
  <<<<<<< Current (Thorium)
  timeout: 300
  =======
  timeout: 600
  >>>>>>> Incoming (Import)
  ...
  ```

  Keep the lines you want, delete the markers, then save and close. Fields shown as `*name*`,
  `*group*`, `*creator*`, `*runtime*`, and `*bans*` are for context only and are ignored. When you
  save, the file is checked: leftover markers, YAML errors (reported with line and column),
  unknown keys (typos), and invalid pipeline triggers are rejected, and you can choose **Edit** to
  fix the file or **Cancel** to abandon the edit. A cancelled edit changes nothing, prints
  `Edit cancelled for image 'static/clamav'`, and asks about the same resource again, so you can
  Edit again, Skip, Apply, or Quit. Deleting the pipeline `triggers` key removes all triggers. If
  the saved result matches Thorium, nothing is sent (`Skipped: No changes detected for …`).
- **Skip** leaves the resource as it is in Thorium.
- **Apply** sends every incoming change for this resource, exactly as `--overwrite` would.
- **Quit** stops processing. No further images or pipelines are handled, and the import moves on
  to [rollback](#rollback).

If an **Edit** or **Apply** fails (the editor exits with an error, or the server rejects the
update), the error is shown and you're asked about the same resource again. You can retry, Skip
it (this isn't counted as a failure), or Quit. There's no "apply to all" choice; to accept every
incoming change without prompting, re-run with `--overwrite`.

In the image-era `images import`, an image's container was pushed right after you chose to apply an
update to it. Skipped images were never pushed.

### What counts as a change

Import, `--skip-conflicts` warnings, the confirmation's `[changed]` tags, the merge prompt, `edit`,
and `toolbox diff` all use the same rules:

- **Omitted fields that keep Thorium's value.** If an incoming config leaves out `sla`
  (pipelines), `security_context`, or `timeout`, or has an empty `network_policies` list, the
  existing value is kept. It isn't reset, and it doesn't count as a change. An existing `kvm`
  config also can't be removed by an import.
- **Omitted fields that clear Thorium's value.** Other optional fields mean what they say. An
  incoming config without a `description`, `version`, `lifetime`, `image`, `clean_up`, or
  `modifiers` clears that field on update.
- **Detected and applied.** Changes to any editable field count, including `args`, `modifiers`,
  `clean_up`, `kvm`, and pipeline `order`. Adding or removing a pipeline stage is a change, and so
  is moving an image to another stage.
- **Ignored.** Descriptions that differ only in trailing whitespace are equal. Set-like fields are
  compared regardless of order: `network_policies`, `child_filters` lists, volumes (matched by
  name), dependency lists, output file names and groups, `env`, and pipeline `triggers`. Real lists
  like `args` and `order` are compared in order.
- **Not editable here.** Name, group, creator, runtime, and bans are never changed by an import or
  an edit. Manage bans with `thorctl images bans` / `thorctl pipelines bans`.
- **Stage tools.** `stage_tools` (the tool each stage runs) is never compared or updated by an
  import or an edit; set a stage's tool with `thorctl pipelines stage-tool`. Adding a stage to an
  existing pipeline's `order` is therefore refused with `400` (every added stage needs a tool),
  while removing a stage drops its tool. A new pipeline takes `stage_tools` from its config, and an
  export wrote the tool ids of the instance it came from, so replace them with ids from the target
  instance first (an unknown id is `404`).
- **Differences an update can't apply** (such as an output handler's `entities` path) are treated
  as unchanged, so they never cause a prompt that could never converge.

### Rollback

Every change an import applies is recorded: created groups, created images and pipelines, and the
previous state of updated ones. If the import **stops early** after changing something, Thorctl
settles that record. Stopping early means Quit in the merge prompt, or a fatal error such as a
failure to create the group.

- **Interactive session:** you're asked (the default is No):

  ```text
  2 changes were applied before the import stopped:
    created group 'static'
    created image 'static/yara'
  Roll back the changes listed above? [y/N]
  ```

- **Can't prompt** (`--overwrite`, `--skip-conflicts`, or no terminal): with
  `--rollback-on-failure` the changes are undone automatically. Without it, each applied change is
  listed as a warning and left in place.

Rollback only undoes Thorium state. Containers already pushed to a registry stay there. A failure
of a single resource (for example a rejected create or a failed push) doesn't stop the import and
isn't rolled back. The other resources keep going, and the failure is reported at the end.

### Registries and containers

A container tarball is **loaded and pushed** only for a K8s image that has a url, has
`images/<name>.tar.gz` in the export, is being created or updated, and isn't covered by
`--skip-push`. Every other image keeps the url from its config (subject to
`--registry-override`).

| Flag | What it changes |
| ---- | --------------- |
| (none) | A pushed container is pushed back to the url it was exported with, and that url is stored |
| `-r, --registry <REG>` | A pushed container is retagged onto `<REG>`, pushed there, and the new url is stored. If an image isn't pushed, a warning says why `--registry` didn't apply, and its url is kept |
| `--registry-override <REG>` | The registry part of **every** image's stored url is replaced, whether or not its container is pushed. Applied after `--registry` |
| `--skip-push` | No container is loaded, retagged, or pushed; `--registry` has no effect |
| `--migrate-registry` | Existing images get a url-only update. Images whose url already matches are skipped, and every other field is left alone. Missing images are created. With `pipelines import`, missing pipelines are created and existing pipelines are left untouched |

Registry rewriting replaces only the leading registry host (the first path component, when it
contains a `.` or `:` or is `localhost`) and keeps the rest of the path. Docker Hub references keep
their namespace:

| Url in the export | `--registry reg.local:5000` |
| ----------------- | --------------------------- |
| `ghcr.io/cisagov/clamav:1.2` | `reg.local:5000/cisagov/clamav:1.2` |
| `myorg/clamav:1.2` | `reg.local:5000/myorg/clamav:1.2` |
| `clamav:1.2` | `reg.local:5000/clamav:1.2` |

Urls are rewritten before resources are compared, so the confirmation screen and the conflict
checks show the urls that will actually be stored. A failed load, retag, or push doesn't stop the
import. That image's create or update is skipped and recorded as a failure (and a new pipeline that
needs it is skipped too), the rest of the import continues, and the command exits non-zero.

#### Container runtime

Loading, tagging, pushing, pulling, and saving use docker or podman. Thorctl picks the runtime in
this order:

1. the global `--container-runtime docker|podman` flag, which goes **before** the subcommand:

   ```bash
   thorctl --container-runtime podman images import -g static -i ./static-export
   ```

2. the `container_runtime: docker` or `container_runtime: podman` setting in your Thorctl config
   (`~/.thorium/config.yml`);
3. auto-detection: docker if `docker --version` works, else podman, else docker (so the error names
   the missing tool).

Log in to the target registry (`docker login` / `podman login`) before an import that pushes.

### Examples

**First import into a new group.** On a terminal, you're shown the plan (including
`New Groups: static`) and asked to confirm:

```bash
thorctl pipelines import -g static -i ./static-export
```

**Import specific images only.**

```bash
thorctl images import -g static -i ./static-export clamav yara
```

**Re-import after the export changed, and review each difference.** Run on a terminal without
`--overwrite` or `--skip-conflicts`. The confirmation marks each existing resource `[changed]` or
`[unchanged]`, then you're asked Edit, Skip, Apply, or Quit for each changed one:

```bash
thorctl images import -g static -i ./static-export --editor "code --wait"
```

**Accept every incoming change.**

```bash
thorctl pipelines import -g static -i ./static-export --overwrite
```

**Only add what's missing, never touching local changes.**

```bash
thorctl pipelines import -g static -i ./static-export --skip-conflicts
```

**CI or other unattended runs.** Without a terminal, nothing prompts. Pick the conflict mode
explicitly (an import that finds differing resources and neither `--overwrite` nor `--skip-conflicts`
exits `1` without changing anything), and add `--rollback-on-failure`
so a run that stops early leaves nothing behind:

```bash
thorctl pipelines import -g static -i ./static-export \
    --overwrite --rollback-on-failure
```

In this case a missing group is created without confirmation, and the job fails (exit code 1) if
any resource failed to import. Without a terminal, Thorctl never asks whether to update itself; if
it's older than the server it only prints a notice on stderr (pass the global `--skip-update` flag
to skip the version check entirely).

**Move to an instance with a different registry.** Load each exported tarball, push it to the new
registry, and store the new urls:

```bash
thorctl images import -g static -i ./static-export -r registry.offline.local:5000/thorium
```

**Push through one hostname, store another.** For example, you push through an external name while
the cluster's nodes pull from an internal one:

```bash
thorctl images import -g static -i ./static-export \
    -r registry.example.com --registry-override registry.cluster.local:5000
```

**The containers are already in place; only import configs.**

```bash
thorctl images import -g static -i ./static-export --skip-push \
    --registry-override registry.cluster.local:5000
```

**A registry moved; repoint existing images without touching anything else.**

```bash
thorctl pipelines import -g static -i ./static-export --skip-push \
    --migrate-registry --registry-override new-registry.example.com
```

### Exit status

| Result | Final line | Exit code |
| ------ | ---------- | --------- |
| Everything applied | `Import complete!` | 0 |
| Declined at the confirmation | `Import cancelled; nothing was changed` | 0 |
| Differing existing resources, no terminal, and neither `--skip-conflicts` nor `--overwrite` | the error naming both flags; nothing was changed | 1 |
| Quit in the merge prompt (after the rollback question) | `Import stopped early`, then `Import stopped early: the remaining resources were not imported` (plus any failures) | 1 |
| Some resources failed | `Import finished with errors`, then e.g. `2 resource(s) failed to import: image 'static/clamav' (container push), pipeline 'static/scan'` | 1 |
| A fatal error (bad config, missing referenced image, group creation failure, …) | the error | 1 |

Differing resources skipped by `--skip-conflicts` don't affect the exit code. Differing resources with
no terminal and neither `--skip-conflicts` nor `--overwrite` do: the import refuses with exit code `1`
before changing anything (see [Conflict modes](#conflict-modes)).

## Editing
---

```text
thorctl images edit    <IMAGE>    [GROUP] [-e <EDITOR>]
thorctl pipelines edit <PIPELINE> [GROUP] [--editor <EDITOR>]
```

`edit` fetches the resource, opens its editable fields as YAML in your editor (in the
[field order](#config-field-order) above), and sends only what you changed. The group is only
needed when another group has a resource with the same name.

| Option | Short | Default | Description |
| ------ | ----- | ------- | ----------- |
| `IMAGE` / `PIPELINE` | | required | The name of the resource to edit |
| `GROUP` | | searched | The resource's group |
| `--editor <EDITOR>` | `-e` | see [Choosing an editor](#choosing-an-editor) | The editor to use |

```bash
thorctl images edit clamav static
thorctl pipelines edit scan --editor nano
```

- For images, `*name*`, `*group*`, `*creator*`, `*runtime*`, and `*bans*` are shown for context.
  Editing them has no effect (a warning lists any you changed). `images edit` stops with the tools
  pointer, because image updates are refused. The pipeline view contains `description`, `sla`,
  `order`, `triggers`, and `stage_tools` (the tool behind each stage).
- Saving an unchanged file prints `No changes detected! Exiting...`.
- If the file doesn't parse, you can choose **Edit** to reopen it or **Cancel** to exit
  (`Cancelled.`, exit 0).
- If the server rejects the update, you can reopen the editor with your edits. If you decline, your
  edits are saved to a file in your temp directory and its path is printed, so nothing is lost.
- `edit` needs a terminal on stdin and stderr, and fails with an error saying so otherwise.

`pipelines edit` runs on the shared edit flow: after you save it shows the changes and asks you to
confirm (`--skip-confirm` or `--yes` skips the question), and it also takes `-g/--group`,
`--file`, `--patch`, `--set`, `--unset`, `--dry-run`, `--force`, and `-o/--format`. The
[Editing Resources](./editing_resources.md) guide covers them, and
[Change a pipeline](./build_pipelines.md#change-a-pipeline) gives pipeline examples.

The same [change rules](#what-counts-as-a-change) apply to images. For example, removing `security_context`
in the editor resets it to the server default.

## Deleting
---

```text
thorctl images delete    -g <GROUP> [--skip-confirm] <IMAGE>...
thorctl pipelines delete -g <GROUP> [--skip-confirm] <PIPELINE>...
```

| Option | Short | Default | Description |
| ------ | ----- | ------- | ----------- |
| `IMAGE...` / `PIPELINE...` | | required | The names to delete (duplicates are ignored) |
| `--group <GROUP>` | `-g` | required | The group to delete from |
| `--skip-confirm` | | off | Don't ask for confirmation |

```bash
$ thorctl images delete -g static clamav yara
Images to delete:
  static/clamav
  static/yara
Delete the images listed above? [y/N] y
Deleted image 'static/clamav'
Warning: image 'static/yara' not found; skipping
```

- The confirmation needs a terminal on stdin and stderr. Without one, pass `--skip-confirm` or the
  command fails with
  `No terminal available to confirm this action; pass --skip-confirm to proceed non-interactively`.
- A name that doesn't exist is warned about and skipped.
- Other failures are reported and the remaining names are still deleted. The command then exits
  non-zero, for example `Failed to delete 1 image(s): 'static/yara'`.

To delete everything a toolbox created, use [`thorctl toolbox remove`](./toolbox.md#removing-a-toolbox).

## Choosing an editor
---

Every Thorctl command that opens an editor (`edit`, the import merge, export `--review` and Merge,
and `toolbox init`/`export`/`import`) picks the editor in this order:

1. the command's `--editor` flag (`-e/--editor` for `edit`);
2. `default_editor` in your Thorctl config, if you changed it from the built-in `vi`
   (`thorctl config --default-editor "code --wait"`);
3. `$VISUAL`;
4. `$EDITOR`;
5. `vi`.

The editor setting is split into a program and arguments the way a shell would, so
`code --wait`, `subl -w`, or `"/opt/My Editor/bin/edit" -w` work. Use an editor command that waits
until the file is closed (for example `code --wait` rather than `code`), because Thorctl reads the
file as soon as the command returns. An editor that exits with an error aborts that edit.

Files being edited are created in a private directory in your temp directory
(`$TMPDIR/thorium-edit-<uuid>/`), readable only by you, and removed afterwards.

## Upgrading
---

See [Upgrading: Changes to Tool Commands](./tool_cli_changes.md) for the behavior and output
changes that can affect scripts written for older Thorctl versions.
