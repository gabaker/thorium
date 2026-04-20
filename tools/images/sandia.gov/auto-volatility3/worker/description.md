# Auto-Volatility3 Worker

The Auto-Volatility3 Worker executes a single Volatility3 plugin against a memory image as dispatched by the Auto-Volatility3 Generator.

## Overview

This worker is the second stage of a two-stage memory analysis pipeline. It receives a Volatility3 plugin name and a memory image path from the generator, then runs that specific plugin via the `vol` command-line tool. Symbol resolution uses a remote ISF (Intermediate Symbol Format) banners URL, allowing analysis of memory images from various OS versions without pre-installing all symbol files. Each worker instance runs one plugin and writes its output as an individual result file.

## Supported File Types

- Memory images / RAM dumps supported by Volatility3 (e.g. raw/`.raw`, `.vmem`, `.lime`, crash dumps, and similar memory capture formats).

This tool operates on memory captures, not arbitrary files. It is intended to be invoked by the generator rather than run against general binaries.

## Usage

This tool is not typically run directly. It is spawned by the Auto-Volatility3 Generator as part of an automated memory forensics pipeline: run it when you have a memory image and want individual Volatility3 plugins executed and their output collected automatically. Because each instance runs a single plugin, the generator fans out across the plugins relevant to the captured image's operating system.

## Documentation

Internal tool. Source: [tools/images/sandia.gov/auto-volatility3/worker](https://github.com/cisagov/thorium/tree/main/tools/images/sandia.gov/auto-volatility3/worker)

## License

Covered by the [Thorium license](https://github.com/cisagov/thorium/blob/main/LICENSE).
