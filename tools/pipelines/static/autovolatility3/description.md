# autovolatility3

Automates memory forensics by running a full suite of Volatility 3 plugins against a submitted memory image in a single pass, surfacing process, network, module, registry, and malware artifacts for rapid triage.

## Images Run

- **autovolatility3** — Runs the H3xKatana autoVolatility3 script, which executes a predefined set of Volatility 3 plugins (full scan) against a memory dump and saves each plugin's output to its own file; bundles the official Volatility 3 symbol packs for Windows, Linux, and macOS.

## Supported File Types

Memory dump files consumable by Volatility 3 (e.g. raw `.dmp` memory images).

## Usage

Run this pipeline on captured memory images when you need fast triage during incident response or memory forensics. A single run produces process lists and trees, command lines, loaded modules and DLLs, network/socket connections, registry data, scheduled tasks, security identifiers, and malware-detection output, making it a good first pass to surface artifacts before deeper manual Volatility 3 analysis.
