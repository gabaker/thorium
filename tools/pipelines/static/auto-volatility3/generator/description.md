# Auto-Volatility3 Generator Pipeline

This pipeline identifies the operating system of a submitted memory image using YARA rules and automatically dispatches the appropriate set of Volatility3 plugin jobs, orchestrating comprehensive memory forensics analysis without manual plugin selection.

## Images Run

- **auto-volatility3** — Custom Rust-based generator that scans a memory image with YARA rules to determine the OS (Windows or Linux), then fans out one `auto-volatility3-worker` sub-reaction per relevant Volatility3 plugin (process, network, registry/filesystem, and malware-detection plugins) for parallel execution.

## Supported File Types

- Memory images / memory dumps (raw physical memory captures, crash dumps)

The pipeline is intended for memory images from Windows and Linux systems; OS detection relies on Windows and Linux YARA rules, and images matching neither rule produce no spawned analysis jobs.

## Usage

Submit captured memory images (memory dumps) when you want automated, comprehensive Volatility3 analysis without manually choosing plugins. It is most useful during incident response and forensic triage of a Windows or Linux memory capture, where it identifies the OS type and orchestrates the appropriate process, network, registry/filesystem, and malware-detection plugins as parallel worker jobs. Run it for any memory image where the operating system is unknown or where running the broad Volatility3 plugin suite is desired.
