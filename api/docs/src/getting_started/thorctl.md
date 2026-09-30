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
- editing, importing, and exporting images and pipelines
- building, importing, and exporting toolboxes

An example of some of these can be found in the [Users](./../users/users.md)
section of these docs. The image, pipeline, and toolbox commands are covered in
[Importing, Exporting, and Editing Images and Pipelines](../developers/import_export.md) and
[Toolboxes](../developers/toolbox.md).

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
  <source src="../static_resources/thorctl-login.mp4", type="video/mp4">
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

Commands that open a text editor (such as `thorctl images edit`) use the editor set with
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
  network-policies  Perform network policy related tasks [alias: netpols]
  ai                Use AI to perform tasks in Thorium
  cart              Cart files locally
  uncart            Uncart files locally
  run               Create and run a reaction, monitor its progress, and download its results
  update            Update Thorctl if necessary
  config            Modify the Thorctl config file indicated by `--config`
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
`thorctl --container-runtime podman images import …`.

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
                                    <GROUP1>/<PIPELINE1>,<GROUP2>/<PIPELINE2>; <PIPELINE>:<GROUP> is
                                    also accepted)
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
