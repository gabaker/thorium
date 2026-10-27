# Overview

Welcome to the Thorium toolbox (curated on behalf of CISA by Sandia National Laboratories)! This portion of the Thorium code repository contains pipeline and image configurations as well as the docker context to build those tools. These configurations and associated toolbox manifests will allow your `thorctl` command-line tool to quickly browse and import existing publicly available tools to your local Thorium deployment. We have built in subcommands to `thorctl` to help generate a Thorium "toolbox" of tools (`toolbox.json`) and build those tool container images using our example [Github actions pipeline](../.github/workflows/tools.yml). Using `thorctl` you too can create your own Thorium tool toolbox!

### Scripts

#### build-actions-matrix.py

The `build-actions-matrix.py` script is used by our gitlab actions pipeline to dynamically generate a set of parallel actions that builds tool container images. These images are stored in the target container registry based on the toolbox configuration contained in `config.toml`. When forking this repository to modify/add your own tools, you will want to update the `config.toml` so that tools get pushed to your forked repository's container registry.

The script reads either toolbox format and prints the same matrix for both. For a legacy toolbox (such as `toolbox.v1.json`) it reads the `images` section. For a current-format toolbox (`toolbox.json`) it reads the `build` section of every tool version (`tools.<name>.versions.<label>.build`) and the `builds` section, which holds build contexts that no tool owns, such as the `ghidra-base` images other builds start from. `test_build_actions_matrix.py` holds its tests:

```bash
python3 -m unittest discover -s tools/scripts -p 'test_*.py'
```

#### Build A Toolbox Manifest 

`toolbox.json` is built from the manifests in this directory, in the current toolbox format (format version 2): `thorctl` imports, diffs, and exports it, and CI builds the tool images from the build entries it holds. To update it before committing changes, run `toolbox build` in this directory. `--use-image-path` tags each image with its manifest's `image_name` (`ghcr.io/cisagov/thorium/tools/<path>`), the tags CI pushes the images to:

```bash
thorctl toolbox build -c config.toml --use-image-path -o toolbox.json
```

CI runs the thorctl test `real_tools_reproduce_the_committed_toolboxes`, which fails when building this directory with the command above does not give `toolbox.json` back byte for byte, or when `toolbox build --rewrite-manifests` would still rewrite a manifest.

#### Older thorctl versions and `toolbox.v1.json`

A `thorctl` older than tool support reads only the legacy format and cannot read `toolbox.json`. `toolbox.v1.json` is the legacy toolbox as it was when this directory's image and pipeline manifests became tool and stage-tool manifests; it is kept for those versions and is not updated, so tools added since are not in it. Converting it (`thorctl toolbox convert toolbox.v1.json`) gives `toolbox.json` as it was at that point.

### Toolbox Config (`config.toml`)

The toolbox configuration file, named `config.toml` in this repo, specifies the name of your toolbox and the fully qualified registry name where your tool container images are stored.

Example:

```toml
name = "CISA Hosted Thorium Github Tools"
registry = "ghcr.io"
```

### Manifests

Tools, base builds, and pipelines contained within the tool box must each have a dedicated `manifest.toml` file. The manifest helps with building the `toolbox.json` file used for building tool images and importing tools and pipelines to your Thorium instance.

##### Tools

A manifest of `type = "tool"` describes a tool by its spec, a JSON file beside the manifest named by `spec_from` (`<name>.json`). `toolbox build` writes a toolbox in the current format when it finds one, and `thorctl toolbox init tool` scaffolds one. The same layout describes toolsets, agents, and workflows (`type = "toolset"`, `"agent"`, `"workflow"`).

```toml
name = "clamav"
type = "tool"
kind = "image"
groups = ["static"]
spec_from = "clamav.json"
build_path = "./"
image_name = "tools/github.com/cisco-talos/clamav"
version = "latest"
```

A tool can run another tool's image with its own spec and build nothing itself: its manifest names the tool in an `[image_from]` table, and its spec leaves the image empty for `toolbox build` to fill in. The `strings-*` tools run the `strings` image this way, each with its own `strings` arguments:

```toml
name = "strings-16le"
type = "tool"
kind = "image"
groups = ["static"]
spec_from = "strings-16le.json"
version = "latest"

[image_from]
name = "strings"
version = "latest"
```

##### Base builds

A manifest of `type = "build"` describes an image that other builds start from but that no tool runs, such as the `ghidra-base` images. It becomes an entry of the toolbox's `builds` section, which is built before the tools:

```toml
name = "ghidra-base"
type = "build"
build_path = "./"
image_name = "tools/ghidra-base"
version = "12.1"
```

##### Pipelines

A pipeline names the tool behind each stage in `[stage_tools.<stage>]` (with `tool` and an optional `version`), and its JSON config is the Thorium pipeline config:

```toml
name = "antivirus"
type = "pipeline"
version = "latest"
config_from = "antivirus.json"

[stage_tools.clamav]
tool = "clamav"
version = "latest"
```

##### Legacy image manifests

The directory's tools were `type = "image"` manifests whose `config_from` named a JSON image config, and its pipelines named images in `[images.<stage>]` tables. `toolbox build` still reads those, and `thorctl toolbox build --rewrite-manifests` rewrote them in place: each image manifest became a tool manifest whose `spec_from` names the same JSON file, which now holds the converted spec; the build-only `ghidra-base` images became `type = "build"` manifests; and each pipeline's `[images.<stage>]` tables became `[stage_tools.<stage>]` tables.
