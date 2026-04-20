# Auto-Volatility3 Worker Pipeline

Executes a single Volatility3 plugin against a memory image as dispatched by the Auto-Volatility3 Generator, collecting that plugin's output as an individual result. It is the worker stage of an automated two-stage memory forensics pipeline.

## Images Run

- **auto-volatility3-worker** — Runs one Volatility3 plugin via the `vol` command-line tool with remote ISF (Intermediate Symbol Format) banner-based symbol resolution, then writes the plugin's output as a result file.

## Supported File Types

Memory images / RAM dumps supported by Volatility3 (e.g. raw/`.raw`, `.vmem`, `.lime`, crash dumps, and similar memory capture formats). This pipeline operates on memory captures, not arbitrary files.

## Usage

Run this pipeline when you have a memory image and want individual Volatility3 plugins executed and their output collected automatically. It is not typically invoked directly; instead it is spawned by the Auto-Volatility3 Generator, which fans out across the plugins relevant to the captured image's operating system. Because each instance runs a single plugin, the generator dispatches one worker per plugin to parallelize analysis.
