# Overview

Welcome to the Thorium toolbox (curated on behalf of CISA by Sandia National Laboratories)! This portion of the Thorium code repository contains pipeline and image configurations as well as the docker context to build those tools. These configurations and associated toolbox manifests will allow your `thorctl` command-line tool to quickly browse and import existing publicly available tools to your local Thorium deployment. We have built in subcommands to `thorctl` to help generate a Thorium "toolbox" of tools (`toolbox.json`) and build those tool container images using our example [Github actions pipeline](../.github/workflows/tools.yml). Using `thorctl` you too can create your own Thorium tool toolbox!

### Scripts

#### build-actions-matrix.py

The `build-actions-matrix.py` script is used by our gitlab actions pipeline to dynamically generate a set of parallel actions that builds tool container images. These images are stored in the target container registry based on the toolbox configuration contained in `config.toml`. When forking this repository to modify/add your own tools, you will want to update the `config.toml` so that tools get pushed to your forked repository's container registry.

The script reads either toolbox format and prints the same matrix for both. For a legacy `toolbox.json` it reads the `images` section. For a current-format toolbox (`toolbox.v2.json`) it reads the `build` section of every tool version (`tools.<name>.versions.<label>.build`) and the `builds` section, which holds build contexts that no tool owns, such as the `ghidra-base` images other builds start from. `test_build_actions_matrix.py` holds its tests:

```bash
python3 -m unittest discover -s tools/scripts -p 'test_*.py'
```

#### Build A Toolbox Manifest 

To update the `toolbox.json` file before committing changes, run the `toolbox build` in this directory. `--use-image-path` tags each image with its manifest's `image_name` (`ghcr.io/cisagov/thorium/tools/<path>`), the tags CI pushes the images to:

```bash
thorctl toolbox build -c config.toml --use-image-path -o toolbox.json
```

The image tags in `toolbox.json` are what the CI builds the tool images from, so it stays in the legacy format. `toolbox.v2.json` is its conversion to the current format, which `thorctl` imports, diffs, and exports. Regenerate it whenever `toolbox.json` changes:

```bash
thorctl toolbox convert toolbox.json -o toolbox.v2.json
```

CI runs `thorctl toolbox convert tools/toolbox.json --check -o tools/toolbox.v2.json`, which fails when `toolbox.v2.json` is not what the conversion produces, and the thorctl test `real_tools_reproduce_the_committed_toolboxes`, which fails when rebuilding this directory with the command above does not give `toolbox.json` back, or when rewriting the manifests (`toolbox build --rewrite-manifests`) and building again does not give `toolbox.v2.json`.

### Toolbox Config (`config.toml`)

The toolbox configuration file, named `config.toml` in this repo, specifies the name of your toolbox and the fully qualified registry name where your tool container images are stored.

Example:

```toml
name = "CISA Hosted Thorium Github Tools"
registry = "ghcr.io"
```

### Manifests

Images and pipelines contained within the tool box must each have a dedicated `manifest.toml` file. The manifest helps with building the `toolbox.json` file used for building tool images and importing pipelines/images to your Thorium instance.

##### Images

```toml
name = "clamav"
type = "image"
config_from = "clamav.json"
build_path = "./"
image_name = "tools/github.com/cisco-talos/clamav"
version = "latest"
```

##### Tools

A manifest of `type = "tool"` describes a tool by a spec file instead of an image config. `toolbox build` writes a toolbox in the current format when it finds one (or when given `--format-version 2`), and `thorctl toolbox init tool` scaffolds one. The same layout describes toolsets, agents, and workflows (`type = "toolset"`, `"agent"`, `"workflow"`).

```toml
name = "clamav"
type = "tool"
kind = "image"
version = "latest"
groups = ["static"]
spec_from = "clamav.spec.yaml"
build_path = "./"
image_name = "tools/github.com/cisco-talos/clamav"
```

`thorctl toolbox build --rewrite-manifests` converts the image manifests of a directory to tool manifests in place, keeping each old `manifest.toml` as `manifest.toml.bak`.

##### Pipelines

```toml
name = "antivirus"
type = "pipeline"
description = "Antivirus scanners (licenses may be required)"
version = "latest"
config_from = "antivirus.json"

[images.clamav]
version = "latest"
```

A pipeline of the current format names a tool for each stage in `[stage_tools.<stage>]` (with `tool` and an optional `version`) in place of `[images.<stage>]`.
