# Auto-Volatility3 Generator

The Auto-Volatility3 Generator is a custom Rust-based generator tool that uses YARA rules to identify the operating system of a memory image and then fans out a set of Volatility3 plugin jobs for automated memory forensics analysis.

## Overview

This generator is the first stage of a two-stage memory analysis pipeline. It scans a submitted memory image with compiled YARA rules to determine whether the image came from a Windows or Linux system, then bulk-creates `auto-volatility3-worker` sub-reactions — one per relevant Volatility3 plugin module — for the detected OS. The Windows module set includes plugins such as `windows.pslist`, `windows.pstree`, `windows.netscan`, `windows.registry.hivelist`, and a broad suite of `windows.malware.*` plugins, while the Linux set includes `linux.pslist`, `linux.lsof`, `linux.mountinfo`, `linux.netfilter`, and many `linux.malware.*` plugins. Each plugin is dispatched as a separate worker job, enabling parallel execution of the full Volatility3 plugin suite without manual plugin selection.

## Supported File Types

- Memory images / memory dumps (raw physical memory captures, crash dumps)

The tool is intended for memory images from Windows and Linux systems; OS detection relies on Windows and Linux YARA rules. Images that match neither rule produce no spawned analysis jobs.

## Usage

Run this generator on captured memory images (memory dumps) when you want automated, comprehensive Volatility3 analysis without manually choosing plugins. It is most useful during incident response and forensic triage of a Windows or Linux memory capture, where it identifies the OS type and orchestrates the appropriate process, network, registry/filesystem, and malware-detection plugins as parallel worker jobs. Submit it for any memory image where the operating system is unknown or where running the broad Volatility3 plugin suite is desired.

## Documentation

Internal tool. Source: [tools/images/sandia.gov/auto-volatility3/generator](https://github.com/cisagov/thorium/tree/main/tools/images/sandia.gov/auto-volatility3/generator)

## License

Covered by the [Thorium license](https://github.com/cisagov/thorium/blob/main/LICENSE).
