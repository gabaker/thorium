# Upgrading: Changes to Tool Commands

This page lists the changes to `thorctl images`, `thorctl pipelines`, and `thorctl toolbox` (plus
related resource targets) that can affect people and scripts upgrading from an older Thorctl. For
how the commands work now, see
[Importing, Exporting, and Editing Images and Pipelines](./import_export.md) and
[Toolboxes](./toolbox.md).

### Exit codes
---

- `images export` and `pipelines export` now exit non-zero when you choose **Quit** at a conflict
  prompt, when a container tarball fails to save, and when the group has nothing to export.
- `images delete` and `pipelines delete` keep going after a failed deletion and exit non-zero at the
  end. A name that doesn't exist is warned about and skipped.
- `images import` and `pipelines import`: a failed container push no longer aborts the import. That
  image is recorded as a failure, the rest continues, and the command exits non-zero.
- `images import`, `pipelines import`, and `toolbox import` now exit non-zero when you choose
  **Quit** in the merge prompt (`Import stopped early: the remaining resources were not imported`).
- `toolbox export` now exits non-zero on **Quit** at a conflict prompt, and when a refresh-all
  (`--overwrite` with no selection) can't fetch some tools. If none can be fetched, it's an error.
- `toolbox export --with-images` exits non-zero when a K8s image has no container url to bundle.
- `toolbox remove` exits non-zero when it leaves images in place because pipelines outside the
  toolbox still use them (these images are no longer attempted). Entries that an import would skip
  are no longer deleted.
- `toolbox diff --exit-code` no longer counts differences that an import can't apply (such as an
  omitted `sla` or `timeout`, or trailing whitespace in a description), so some runs that used to
  exit `1` now exit `0`.
- Cancelling a config in the `toolbox init` editor exits non-zero, and nothing is written for that
  tool.

### Change detection during import and diff
---

- If an incoming config omits `sla`, `security_context`, or `timeout`, or has an empty
  `network_policies` list, the existing values are kept. They used to be reset or stripped.
- Changes to `args`, `modifiers`, `clean_up`, and `kvm`, and adding or removing pipeline stages,
  are now detected and applied. They used to be ignored.
- Descriptions that differ only in trailing whitespace are no longer treated as changes.
- In the interactive merge, a failed **Apply** or **Edit** now asks about the same resource again
  instead of aborting. Skipping it afterwards isn't counted as a failure.
- In the merge editor, unknown keys and invalid triggers are rejected when you save, and deleting
  the `triggers` key is allowed. Cancelling an edit prints `Edit cancelled for …` and asks about
  the same resource again.

### Stricter validation
---

These now fail up front, before anything is changed:

- `images import` / `pipelines import`: a config whose `name` doesn't match its file name, and a
  pipeline with an empty `order` or an empty stage.
- A merged `.json` file (export **Merge**) that uses YAML-only syntax.
- Image and pipeline targets (`images describe`, `pipelines describe`, `reactions create -p`,
  `files upload -p`): an empty name or group (`:static`, `scan:`), and targets that mix `/` and `:`.
- `toolbox export`: an empty group or name in `-i`/`-p`; tools with the same name and version from
  different groups, or two tools written to the same directory; one resource given two `=dir`
  destinations; unsafe `export_image_path`/`export_pipeline_path` values in `config.toml`; and
  `--strip-registry` without a registry (checked before anything is fetched).
- `toolbox init`: invalid names or `--group` (before any file is written with `-n`; interactively
  you're asked for a new name), duplicate tool directory names, a missing or invalid `-c` config for
  `init image`/`init pipeline`, and absolute or `..` export paths in an `init toolbox --config`
  template.
- Toolbox manifest locations: URL schemes other than `http(s)://` are rejected. `file://` URLs are
  read locally, and Windows drive paths (`C:\…`) are treated as paths.
- `toolbox import`: when several remote `config_from`/`network_policies_from` files fail to load,
  one error lists all of them (nothing is changed).
- Network policies bundled with no groups are skipped with a warning.

Some checks are now more lenient: manifests may omit `description`, `images`, and `build_path`, and
`toolbox init pipeline --order` accepts a flat list (`["clamav","yara"]`) as well as stages. A
pipeline import may reference an image that's missing from the export if it already exists in the
target group.

### Terminals and prompts
---

- Prompts now require a terminal on both stdin **and** stderr. A run with stderr redirected (for
  example `2>&1 | tee log.txt`) is non-interactive: `toolbox import` collisions default to Skip, the
  confirmation is skipped, and a bundled toolbox needs `--image-path-prefix`.
- `images edit` and `pipelines edit` require a terminal.
- `--review` without a terminal is an error (`images export`, `pipelines export`,
  `toolbox export`). `toolbox export` used to warn and export without review.
- `toolbox init` requires a terminal unless you pass `-n` (with `--group`, and for
  `init pipeline` also `--images`).
- No command asks about updating Thorctl when there's no terminal; it prints the out-of-date notice
  instead.
- `-q/--quiet` now hides progress and informational output for `images export`,
  `pipelines export`, and `toolbox export` (warnings and errors still print).
- An import that will create a group now asks for confirmation (interactive `images import` and
  `pipelines import`).

### Changed behavior and defaults
---

- `images import`, `pipelines import`, and `toolbox import` (bundled toolboxes) push containers only
  for images that are created or updated, never for unchanged or skipped ones. A failed push skips
  that image's create or update instead of aborting the import.
- `toolbox export --with-images` bundles only K8s images (others run without a container and are
  exported without a tarball) and uses the configured container runtime (docker or podman).
  `toolbox import` and `toolbox diff` leave the urls of non-K8s bundled images unchanged.
- `--registry` and `--registry-override` keep Docker Hub namespaces: `myorg/tool:1.0` becomes
  `<registry>/myorg/tool:1.0`.
- `--migrate-registry` skips images whose url already matches and collects failures. In
  `pipelines import` it leaves existing pipelines untouched.
- `images export` / `pipelines export` save container tarballs only for K8s images. After a Quit,
  `pipelines export` still writes the image configs and tarballs that the pipelines already on disk
  need.
- `toolbox init toolbox` resolves relative `-i`/`-p` paths under `--toolbox-dir`: with
  `--toolbox-dir tb`, pass `-i images/clamav`, not `-i tb/images/clamav` (which now means
  `tb/tb/images/clamav`).
- `--group-override` rewrites the groups of bundled network policies (`toolbox import`, `remove`,
  and `diff`).
- `toolbox export` settings precedence: a kept `<output>/config.toml` wins over a different
  `--config` (with a warning), and `--overwrite-config` applies `--name` and `--registry`. `--name`
  no longer has a fixed default: `My Toolbox` is used only for a new toolbox, and a `--name` that
  differs from a kept `config.toml` is warned about and ignored.
- `toolbox export`: `=dir` on a pipeline no longer moves a same-named auto-included image, and
  destinations are scoped per group. `--overwrite` updates `version`, `exported_image_path`, and
  `network_policies_from` in an image's `manifest.toml`. For a pipeline, it keeps the manifest's
  `version` and `description` and replaces its `[images.*]` tables. Unchanged tools aren't rewritten,
  and `--strip-registry` keeps `image: null` for images with no url.
- `toolbox build` includes a `manifest.toml` directly in the crawl root, takes a pipeline's
  description from `description.md`, and gives unbuilt non-K8s images without a pinned url
  `image_tags = []` instead of a derived url.
- `toolbox build-images --tag-suffix` doesn't re-suffix a tag that already ends with the suffix.
- Editors: `$VISUAL` and `$EDITOR` are honored when `default_editor` isn't customized, and editor
  settings with arguments (such as `code --wait`) work.
- **Server:** a pipeline created without an `sla` now gets 604800 seconds (1 week). A typo used to
  make the default 640800 seconds (about 7.4 days). Existing pipelines keep their stored SLA.

### Output text
---

Scripts that match on output may need updating:

- Resources are named `group/name` (for example `Deleted image 'static/clamav'`). Toolbox manifest
  entries are named `group/name@version` (for example `Unchanged: image 'static/clamav@1.2'`), and
  import confirmation lists no longer carry a ` (group: g)` suffix. Network policies are named
  `name (group: g, id: X)`, `name (id: X)`, or `name (groups: a, b)`, and group lists print as
  `[a, b]`. Ambiguity errors list candidates as `group/name`.
- Error messages that started with `Error …` now read `Failed to …`.
- Completion messages: `Done!` became `Export complete!`, `Init complete!`, and
  `Image build complete!` (or `Image build finished with errors:`).
- Imports end with `Import complete!`, `Import finished with errors`, or `Import stopped early`,
  and failures are summarized as `N resource(s) failed to import: …`. Declining a confirmation
  prints `Import cancelled; nothing was changed`.
- The merge editor labels the imported side `Incoming (Import)`.
- `toolbox diff` headers read `<host>/<group>/<name> (image)` and
  `toolbox/<group>/<name> (image|pipeline)`, and hunks follow the standard config field order
  instead of alphabetical order.
- Warnings include the command name (`Warning: toolbox import - …`). `toolbox init` and
  `toolbox build` print warnings on stderr.
- `toolbox remove` always prints its plan and per-resource lines on stdout, even with
  `--skip-confirm` or without a terminal, and the plan's layout changed.
- When progress bars are hidden (stderr isn't a terminal), informational lines go to stdout and
  errors from background workers go to stderr instead of being dropped.

### Files
---

- Exported configs use a new key order that matches the Web UI, and set-valued arrays are sorted.
  The first `images export` / `pipelines export` over files written by an older Thorctl reports
  each of them as a conflict once. See
  [Re-exporting over an existing directory](./import_export.md#re-exporting-over-an-existing-directory).
- Temporary edit files are created in a private per-session directory,
  `$TMPDIR/thorium-edit-<uuid>/`, readable only by you.
