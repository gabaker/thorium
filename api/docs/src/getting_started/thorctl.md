# Thorctl
---
Thorctl is a command line tool aimed at enabling large scale operations within Thorium. Thorctl provides a variety
of features including:

- uploading files
- uploading Git repositories
- ingesting Git repositories by URL
- downloading files/repos
- starting reactions/jobs
- starting Git repo builds
- downloading results
- listing files
- searching the contents of files, repos, and entities with a regex (`thorctl grep`; see [Search](../users/search.md#grepping-with-thorctl))
- creating groups (`thorctl groups create`)
- creating, publishing, editing, deleting, and running tools (`thorctl tools`, `thorctl instances`)
- creating, editing, and running pipelines (`thorctl pipelines`)
- building and running AI workflows (`thorctl agents`, `thorctl toolsets`, `thorctl workflows`,
  `thorctl runs`), and changing any of these in your editor or from a script
- managing model endpoints, credentials, profiles, and your own model settings (`thorctl models`)
- validating, converting, building, importing, exporting, diffing, and removing toolboxes of tools and
  pipelines (`thorctl toolbox`)
- exporting and importing pipelines, and reading images (images can no longer be written)

An example of some of these can be found in the [Users](./../users/users.md)
section of these docs. Tools, agents, toolsets, workflows, and pipelines are covered in
[Tools and Tool Instances](../developers/tools.md),
[AI Agents, Toolsets, and Workflows](../developers/agents_workflows.md), and
[Building Pipelines](../developers/build_pipelines.md); running them in
[Running Tools and Workflows](../users/runs.md); changing them in
[Editing Resources](../developers/editing_resources.md); and toolboxes in
[Toolboxes](../developers/toolbox.md). Pipeline and image import and export are covered in
[Importing, Exporting, and Editing Images and Pipelines](../developers/import_export.md).

To install Thorctl, follow the instructions for your specific operating system in the sections below.

### Linux/Mac

On a Linux or Mac machine, open a terminal window and run the following command:

<script>
  let base = window.location.origin;
  document.write("<pre>");
  document.write("<code class=\"language-bash  hljs\">");
  document.write("curl " + base + "/api/binaries/install-thorctl.sh | bash -s -- " + base);
  document.write("</code>");
  document.write("</pre>");
</script>

  #### Insecure Download

  - Although not recommended, you can bypass certificate validation and download Thorctl insecurely
    by adding the `-k` (insecure) flag to `curl` and `--insecure` at the very end of the command
    (see the command below for reference). The former tells `curl` to download the script itself
    insecurely while the latter will informs the script to use insecure communication when downloading
    Thorctl.

<script>
  document.write("<pre>");
  document.write("<code class=\"language-bash  hljs\">");
  document.write("curl -k " + base + "/api/binaries/install-thorctl.sh | bash -s -- " + base + " --insecure");
  document.write("</code>");
  document.write("</pre>");
</script>

### Windows

Download Thorctl from the following link: [Windows Thorctl](../../../binaries/windows/x86-64/thorctl.exe)


### Login Via Thorctl

After you have downloaded Thorctl, you can authenticate by running:

<script>
  document.write("<pre>");
  document.write("<code class=\"language-bash  hljs\">");
  document.write("thorctl login " + base);
  document.write("</code>");
  document.write("</pre>");
</script>

<video autoplay loop controls>
  <source src="../static_resources/thorctl-login.mp4" type="video/mp4">
</video>

### Configure Thorctl

Logging into Thorium using `thorctl login` will generate a Thorctl config file containing the
user's authentication key and the API to authenticate to. By default, the config is stored
in `<USER-HOME-DIR>/.thorium/config.yml`, but you can manually specify a path like so:

```
thorctl --config <PATH-TO-CONFIG-FILE> ...
```

The config file can also contain various other optional Thorctl settings. To easily modify the config
file, use `thorctl config`. For example, you can disable the automatic check for Thorctl updates
by running:

```
thorctl config --skip-update=true
```

You can specify a config file to modify using the `--config` flag as described above:

```
thorctl --config <PATH-TO-CONFIG-FILE> config --skip-update=true
```

To skip the update check for a single command instead, pass the global `--skip-update` flag before the subcommand
(for example `thorctl --skip-update images get`). When Thorctl is older than the server it asks whether to update
only if it's running in a terminal; in scripts and CI jobs it just prints an out-of-date notice on stderr and carries on.

Commands that open a text editor (such as `thorctl agents edit` or `thorctl pipelines edit`) use the editor set with
`thorctl config --default-editor <EDITOR>`, falling back to `$VISUAL`, `$EDITOR`, and finally `vi`
(see [Choosing an editor](../developers/import_export.md#choosing-an-editor)). Commands that pull, push, or build container
images use docker or podman; set `container_runtime: docker` or `container_runtime: podman` in the config file, or pass the
global `--container-runtime` flag, to choose one explicitly.

#### Proxy Settings

By default Thorctl **respects proxy settings from the environment**
(`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`, honoring `NO_PROXY`). You can override this in the
`client` block of the config:

```yaml
client:
  # Disable all proxies and connect directly, ignoring the environment (default: false)
  disable_proxy: false
  # Route all traffic through an explicit proxy (may embed credentials,
  # e.g. http://user:pass@host:port). Takes precedence over the environment.
  proxy: http://proxy.example.com:8080
  # Comma-separated hosts/domains/CIDRs that bypass the explicit `proxy`
  # (mirrors NO_PROXY; only applies when `proxy` is set).
  no_proxy: localhost,cluster.local
```

These can also be set with `thorctl config`:

```
thorctl config --proxy http://proxy.example.com:8080 --no-proxy localhost,cluster.local
thorctl config --disable-proxy=true
thorctl config --clear-proxy
```

### Command Overview

| Command | What it does | Guide |
| ------- | ------------ | ----- |
| `files`, `repos`, `tags`, `results`, `associations`, `trees` | Upload, tag, search, and download data | [Users](../users/users.md) |
| `groups` | List, describe, and create groups | [Creating/Editing Groups](./adding_editing_groups.md) |
| `tools`, `instances` | Create, publish, edit, update, delete, snapshot, and run tools; list and compare instances; `instances request` builds one now | [Tools and Tool Instances](../developers/tools.md) |
| `agents`, `toolsets`, `workflows` | Create, link, spec, update, edit, delete, and run AI agents, toolsets, and workflows | [AI Agents, Toolsets, and Workflows](../developers/agents_workflows.md) |
| `pipelines` | Create, update, edit, run, export, and import pipelines and set each stage's tool | [Building Pipelines](../developers/build_pipelines.md) |
| `run`, `runs`, `reactions` | `run tool\|workflow\|pipeline` starts a run (one run per input with `--one-per-input`, tagged with `--batch`); `runs` lists, follows, reads logs, retries, and cancels runs singly or by batch | [Running Tools and Workflows](../users/runs.md) |
| `models` | Endpoints, catalogs, profiles, credentials, bans, and your own model settings | [Models](../users/models.md) |
| `completions` | Print a tab-completion script for bash, zsh, fish, PowerShell, or elvish | [Shell completions](#shell-completions) |
| `toolbox` | Validate, list, convert, build, import, update, export, diff, and remove toolboxes | [Toolboxes](../developers/toolbox.md) |
| `images` | Read images (writes are refused; use tools) | [Importing, Exporting, and Editing Images and Pipelines](../developers/import_export.md) |
| `edit` and `update` commands | Change a resource in your editor or from flags, files, `--set`, and `--patch` | [Editing Resources](../developers/editing_resources.md) |

The `-o/--format` flag (`table`, `human`, `json`, or `yaml`) on the list, describe, and run commands, and
`--watch` and `--wait-for` on most of them, are described in [Tools and Tool
Instances](../developers/tools.md#inspecting-tools).

### Using Thorctl in scripts
---

- Authenticate without a prompt with a keys file (`thorctl --keys <FILE> ...`, or `thorctl login -u <USER> -p
  <PASSWORD>` once to write a config) and set `thorctl config --skip-update=true` so no command checks for updates.
- Confirmation prompts need a terminal on stdin and stderr. In a script pass `--skip-confirm` (alias `--yes`) to a
  command that asks, and `-q/--quiet` to drop progress output.
- List, describe, and run commands print JSON or YAML with `-o json` or `-o yaml` (see [Tools and Tool
  Instances](../developers/tools.md#inspecting-tools)); read them with `jq` rather than parsing tables.
- Run commands with a wait flag use the exit codes `0` (completed), `1` (failed or refused), `2` (bad flags or input),
  and `3` (still unfinished when the wait ended); a batch (`--one-per-input`) exits `1` when any run failed or did not start,
  which wins over `3`, and `-o json` prints one JSON line per input; see [Running Tools and Workflows](../users/runs.md#flags-every-run-command-shares).
- Colors and styling appear only on a terminal: Thorctl writes plain text to pipes and files, honors `NO_COLOR` and
  `CLICOLOR`, and hides progress bars on `TERM=dumb`.
- A command that needs a terminal for a question and has none stops with a message naming the flag that answers it
  (`--skip-confirm`/`--yes`, `--force`, `login --user`/`--password`, or `thorctl config --skip-insecure-warning=true`)
  instead of waiting. Ctrl-C at a prompt restores your terminal. While an editor is open Ctrl-C belongs to the editor,
  and if Thorctl itself is interrupted it removes the temporary directory that held the document.
- The routes behind the commands are listed on each guide page and in the
  [REST API Reference](../architecture/api.md#rest-api-reference).

### Shell completions

`thorctl completions <SHELL>` prints a tab-completion script for `bash`, `zsh`, `fish`, `powershell`, or `elvish` to
stdout. The script is generated from Thorctl's own command tree, so it always lists the commands and flags of the
version that printed it. The command needs no config file and no login, so you can run it before you log in. Install
the script once for your shell, and print it again after you update Thorctl.

**bash**

```bash
mkdir -p ~/.local/share/bash-completion/completions
thorctl completions bash > ~/.local/share/bash-completion/completions/thorctl
```

**zsh** (any directory on your `$fpath`, with the file named `_thorctl`)

```bash
mkdir -p ~/.zfunc
thorctl completions zsh > ~/.zfunc/_thorctl
```

Add `fpath=(~/.zfunc $fpath)` to `~/.zshrc` before the line that runs `compinit`.

**fish**

```bash
mkdir -p ~/.config/fish/completions
thorctl completions fish > ~/.config/fish/completions/thorctl.fish
```

**PowerShell** (adds the script to your profile, which `$PROFILE` names)

```powershell
New-Item -Force -ItemType File $PROFILE
thorctl completions powershell >> $PROFILE
```

**elvish**

```bash
mkdir -p ~/.config/elvish/lib
thorctl completions elvish > ~/.config/elvish/lib/thorctl.elv
```

Add `use thorctl` to `~/.config/elvish/rc.elv`.

Start a new shell afterwards so it loads the script. `thorctl completions --help` repeats these steps.

### Thorctl Help

Thorctl will print help info if you pass in either the `-h` or `--help` flags.

```bash
$ thorctl -h
The command line args passed to Thorctl

Usage: thorctl [OPTIONS] <COMMAND>

Commands:
  clusters          Manage Thorium clusters
  login             Login to Thorium interactively
  groups            Perform group related tasks
  files             Perform file related tasks
  images            Perform image related tasks
  pipelines         Perform pipeline related tasks
  reactions         Perform reaction related tasks
  results           Perform result related tasks
  tokens            Perform scoped token related tasks
  activate          Activate a scoped token making Thorctl authenticate with it
  deactivate        Deactivate the currently activated scoped token
  tags              Perform tag related tasks
  repos             Perform repository related tasks
  trees             Perform tree related tasks
  associations      Create, delete, and list associations between files, repos, and entities [alias:
                    assoc]
  network-policies  Perform network policy related tasks [alias: netpols]
  tools             Create, publish, and inspect tools and their versions
  models            Manage model endpoints, catalogs, profiles, credentials, and their bans
  agents            Create, link, and inspect AI agents (not the thorium-agent job runner)
  toolsets          Create, edit, and inspect toolsets of agentic tools that AI agents attach as one
                    unit
  workflows         Create, edit, and inspect workflows that wire AI agents together
  instances         List, describe, and compare tool, toolset, and workflow instances
  runs              List, describe, retry, and cancel runs, singly or by batch
  cart              Cart files locally
  uncart            Uncart files locally
  run               Start a tool, workflow, or pipeline run (`run <KIND> <NAME>`), or run one
                    pipeline on one input
  update            Update Thorctl if necessary
  config            Modify the Thorctl config file indicated by `--config`
  completions       Print a shell completion script to stdout
  toolbox           Perform toolbox related tasks
  help              Print this message or the help of the given subcommand(s)

Options:
      --admin <ADMIN>
          The path to load the core Thorium config file from for admin actions [default:
          ~/.thorium/thorium.yml]
      --config <CONFIG>
          The path to authentication key files for regular actions [default:
          ~/.thorium/config.yml]
      --keys <KEYS>
          The path to a keys file to used to authenticate with the Thorium API
      --skip-update
          Don't check for updates from the API
  -w, --workers <WORKERS>
          The number of parallel async actions to process at once [default: 10]
  -q, --quiet
          Disable progress tracking and only print errors to stderr
      --container-runtime <CONTAINER_RUNTIME>
          Container CLI to use for image pull/save/load/tag/push/build [possible values: docker,
          podman]
  -h, --help
          Print help (see more with '--help')
  -V, --version
          Print version
```

These global options go **before** the subcommand, for example `thorctl --workers 20 files upload …` or
`thorctl --container-runtime podman toolbox build-images ./toolbox.json`.

Each subcommand of Thorctl (eg `files`) has its own help menu to inform users on the available options for that
subcommand.

```bash
$ thorctl files upload --help
Upload some files and/or directories to Thorium

Usage: thorctl files upload [OPTIONS] --groups <FILE_GROUPS> <TARGETS|--from-file <FROM_FILE>>

Arguments:
  [TARGETS]...  The files and/or folders to upload

Options:
      --from-file <FROM_FILE>       An optional file containing a list of paths to files/directories
                                    to upload, delimited by newline
  -g, --groups <FILE_GROUPS>        The groups to upload these files to
  -T, --file-tags <FILE_TAGS>       The tags to add to any files uploaded where key/value is
                                    separated by a delimiter
      --delimiter <DELIMITER>       The delimiter character to use when splitting tags into
                                    key/values
                                       (i.e. <TAG>=<VALUE1>=<VALUE2>=<VALUE3>) [default: =]
      --dry-run                     Display files that will be uploaded without uploading them
  -p, --pipelines <GROUP/PIPELINE>  Any pipelines to immediately spawn for the files that are
                                    uploaded; pipelines are specified by their group + name,
                                    separated with "/" (i.e.
                                    `<GROUP1>/<PIPELINE1>,<GROUP2>/<PIPELINE2>`;
                                    `<PIPELINE>:<GROUP>` is also accepted)
      --workflows <[GROUP/]WORKFLOW[#rN]>
                                    Any workflows to run on each file that is uploaded
      --tools <[GROUP/]TOOL[@VERSION]>
                                    Any tools to run on each file that is uploaded
      --batch <NAME>                Tag every run started with this name
      --wait                        Block until every run started is Completed or Failed
      --watch                       Follow the runs started until they finish with no timeout
      --timeout <TIMEOUT>           How long `--wait` follows the runs before giving up
                                    [default: 10m]
      --interval <INTERVAL>         How long to wait between reads of the runs [default: 2s]
  -o, --format <FORMAT>             The format to print results in: human (a table) or json
  -f, --filter <FILTER>             Any regular expressions to use to determine which files to
                                    upload
  -s, --skip <SKIP>                 Any regular expressions to use to determine which files to skip
      --include-hidden              Include hidden directories/files
      --folder-tags <FOLDER_TAGS>   A list of keys to assign to directory names to upload as tags
                                    (e.g. --folder-tags "a/b" on "foo/bar/baz.txt" would have tags
                                    "a=foo", "b=bar")
  -h, --help                        Print help (see more with '--help')
  -V, --version                     Print version
```
