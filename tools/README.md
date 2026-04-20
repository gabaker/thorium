# Overview

Welcome to the Thorium toolbox (curated on behalf of CISA by Sandia National Laboratories)! This portion of the Thorium code repository contains pipeline and image configurations as well as the docker context to build those tools. These configurations and associated toolbox manifests will allow your `thorctl` command-line tool to quickly browse and import existing publicly available tools to your local Thorium deployment. We have built in subcommands to `thorctl` to help generate a Thorium "toolbox" of tools (`toolbox.json`) and build those tool container images using our example [Github actions pipeline](../.github/workflows/tools.yml). Using `thorctl` you too can create your own Thorium tool toolbox!

### Scripts

#### build-actions-matrix.py

The `build-actions-matrix.py` script is used by our gitlab actions pipeline to dynamically generate a set of parallel actions that builds tool container images. These images are stored in the target container registry based on the toolbox configuration contained in `config.toml`. When forking this repository to modify/add your own tools, you will want to update the `config.toml` so that tools get pushed to your forked repository's container registry.

#### Build A Toolbox Manifest 

To update the `toolbox.json` file before committing changes, run the `toolbox build`:

```bash
thorctl toolbox build --path './' -c config.toml -o toolbox.json
```

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
