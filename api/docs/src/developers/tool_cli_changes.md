# Upgrading: Changes to Tool Commands

This page lists the changes to `thorctl images`, `thorctl pipelines`, and `thorctl toolbox` (plus
related resource targets) that can affect people and scripts upgrading from an older Thorctl. For
how the commands work now, see
[Importing, Exporting, and Editing Images and Pipelines](./import_export.md) and
[Toolboxes](./toolbox.md).

Images are read-only, so a bullet below that names `images import`, `images edit`, or image bans
describes a step those commands no longer reach: they are refused first (see
[Images become tools](#images-become-tools)). The `toolbox` commands work on tools, and where a bullet
below and [Toolboxes](./toolbox.md) disagree about a `toolbox` command, the Toolboxes page is right.

### Images become tools
---

- Images can no longer be created or updated: the API returns `410 Gone`, so `images import` and
  `images edit` stop before they open an editor or push a container, with `Images can no longer be
  created or updated; use tools (thorctl tools create|edit|publish)`. Image bans are refused the same
  way. Use `thorctl tools create` and `thorctl tools publish` instead (see
  [Tools and Tool Instances](./tools.md)).
- `toolbox import` works again, for both toolbox formats. A legacy image toolbox is converted to
  tools in memory, and `toolbox convert` writes the converted file (see [Toolboxes](./toolbox.md)).
- Every pipeline stage needs a tool; set it with `thorctl pipelines create --stage-tool`,
  `pipelines update --stage-tool`, or `pipelines stage-tool`.
- `--format yaml` output is the same document as `--format json`, written as YAML.

### Pipelines and runs
---

- `pipelines export` writes each stage's tool as `{tool, version}` in `stage_tools`, and
  `pipelines import` reads it and looks the tool up by name in the target group. Both stopped
  handling images: `--config-only`, `--registry`, `--registry-override`, `--skip-push`, and
  `--migrate-registry` are accepted and ignored. An export made before this release has no
  `stage_tools`, so importing it fails before any request, naming the stages that need a tool.
- `pipelines edit` now asks you to confirm the changes after an editor session, as every `edit`
  command does (`--skip-confirm` or `--yes` skips the question). It used to apply them directly. It
  also takes `-g/--group`; the short `-e` for `--editor` still works.
- `tools run --output <DIR>` implies `--wait`, and the wait ends after `--timeout` (10 minutes by
  default) with exit code `3`. It used to wait without a limit.
- A tool run prints one JSON object unless you ask to follow it (`--logs`, `--watch`, `--wait-for`, or
  `-o` next to `--wait` or `--output`). See [Running Tools and Workflows](../users/runs.md).
- `thorctl run tool|workflow|pipeline <NAME>` starts a run of any kind with the flags of `tools run`, `workflows run`, and
  `pipelines run`, which keep working. The older `thorctl run <PIPELINE> <SHA256|REPO>` still works when its first word is
  not a kind. Every run command gains `--one-per-input`, `--batch`, and `--dry-run`, and `--sample-list -` and
  `--repo-list -` read stdin (see [Batch runs](../users/runs.md#batch-runs)).
- `runs get` gains `--batch` and `--status`, and `runs cancel` and `reactions fail` can cancel by `--batch`,
  `--workflow`, `--status`, and `--group`, with one confirmation for the set.
- `workflows run` takes a workflow name (its latest Ready instance) as well as `<workflow>#r<N>` and an
  instance id.

### Exit codes
---

- `images export` and `pipelines export` now exit non-zero when you choose **Quit** at a conflict
  prompt, when a container tarball fails to save, and when the group has nothing to export.
- `images delete` and `pipelines delete` keep going after a failed deletion and exit non-zero at the
  end. A name that doesn't exist is warned about and skipped.
- `pipelines import` now exits non-zero when you choose **Quit** in the merge prompt (`Import
  stopped early: the remaining resources were not imported`). Choosing **Quit** in `toolbox import`
  writes nothing and exits `0`.
- `toolbox export` stops before writing anything, and exits non-zero, when an item already in the
  output differs and neither `--overwrite` nor `--skip-conflicts` is given.
- `toolbox export --with-images` exits non-zero when a K8s image has no container url to bundle.
- `toolbox remove` exits `1` (`not fully removed`) when it leaves a tool in place because a pipeline
  outside the toolbox runs it, or when a delete is refused or fails.
- `toolbox diff --exit-code` exits `1` when an import would write anything; items only in the
  instance and tool roots (description, tags, groups) never count.
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
  example `2>&1 | tee log.txt`) is non-interactive: `toolbox import` refuses before any write when
  items differ and neither `--overwrite` nor `--skip-conflicts` is given, and otherwise skips the
  confirmation.
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
- Ctrl-C (or a termination signal) at a prompt or menu restores the terminal (cursor and echo) and ends Thorctl as an
  interrupt (exit code `130` when the prompt reports the key press). While an editor is open Ctrl-C belongs to the
  editor; if Thorctl is interrupted or the terminal closes meanwhile, it removes the temporary directory that held
  the document.
- A question with no terminal to ask it no longer prints an operating-system error: `trees delete` names `--force`
  (`--yes`), the insecure-connection question names `thorctl config --skip-insecure-warning=true`, and `login`
  names `--user` and `--password`. `login` refuses an empty username or password and an input that ends early.
- Output is colored only on a terminal. Tables, logs, notifications, and `describe` and `diff` output are plain text in
  pipes and files, honor `NO_COLOR` and `CLICOLOR`, and progress bars are hidden on `TERM=dumb`.
- Without a terminal, every command that would have to ask about a conflict or a deletion now refuses the same way:
  it exits `1` with one line that names the flag to pass, and changes nothing. `images import` and `pipelines import`
  (an existing resource differs) and `images export` and `pipelines export` (an on-disk config differs) now do this
  too, naming `--skip-conflicts` and `--overwrite`; they used to skip the differing items with a warning and exit `0`.
  With `--skip-conflicts` they skip the differing items and print one summary line saying how many were skipped
  (also with `-q`) and exit `0`.
- `thorctl tokens delete` and `thorctl network-policies delete` now ask for confirmation (default no; the network
  policy question names the groups that hold the policy) and take `--skip-confirm` (alias `--yes`).
  `files delete`, `reactions delete`, and `tags delete` still delete without asking.
- Answering no to the `runs cancel` and `reactions fail` question prints `Cancelled; nothing was changed.` on stderr
  and exits `0`.
- `--set`, `--file`, and `--patch` edits apply without a confirmation on a terminal; only an editor session asks.
- A question without a default answer (the insecure-connection `Continue?` and the import plan) ignores Enter and waits
  for `y` or `n`; the others take their default (no for deletions, yes for applying).
- In a prompt, Ctrl-D, Ctrl-Z, and Escape are ignored and only Ctrl-C leaves; end of input counts only when stdin is not
  a terminal, where every confirmation refuses.
- Characters typed before a password prompt has put the terminal in no-echo mode are discarded, as with any password
  prompt.

### Changed behavior and defaults
---

- `toolbox import` never pushes containers, bundled or not: push a bundled toolbox's images to the
  registry yourself (see [Offline transfer with bundled images](./toolbox.md#offline-transfer-with-bundled-images)).
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

- Resources are named `group/name` (for example `Deleted image 'static/clamav'`), and import
  confirmation lists no longer carry a ` (group: g)` suffix. `toolbox import` names each item by
  kind and name (for example `created tool strings`) and ends with a `Result:` line that counts them. Network policies are named
  `name (group: g, id: X)`, `name (id: X)`, or `name (groups: a, b)`, and group lists print as
  `[a, b]`. Ambiguity errors list candidates as `group/name`.
- Error messages that started with `Error …` now read `Failed to …`.
- Completion messages: `Done!` became `Export complete!`, `Init complete!`, and
  `Image build complete!` (or `Image build finished with errors:`).
- `pipelines import` ends with `Import complete!`, `Import finished with errors`, or `Import stopped early`,
  and failures are summarized as `N resource(s) failed to import: …`. Declining a confirmation
  prints `Import cancelled; nothing was changed`.
- The merge editor labels the imported side `Incoming (Import)`.
- `toolbox diff` hunk headers read `live/<kind> <name>` and `toolbox/<kind> <name>`.
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
