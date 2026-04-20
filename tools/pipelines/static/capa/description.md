# capa

Detects the capabilities of executable files by matching their behaviors against capa's community-contributed rule set, mapping results to the MITRE ATT&CK framework and the Malware Behavior Catalog.

## Images Run

- **capa** — Runs capa's static analysis against the submitted file to identify behaviors and capabilities (such as C2/HTTP communication, persistence via services, data encoding, and process interaction), maps them to ATT&CK tactics/techniques and MBC, and flags samples that appear to be packed.

## Supported File Types

- PE executables
- ELF executables
- .NET modules
- Shellcode files
- Sandbox analysis reports (e.g. CAPE, DRAKVUF, VMRay) for dynamic capability detection

## Usage

Run this pipeline for initial triage of suspicious or unknown executables to quickly determine what a program is capable of without manual reverse engineering. It is well suited to PE, ELF, .NET, and shellcode samples, and reports ATT&CK tactics/techniques alongside fine-grained capability matches like "connect to HTTP server" or "persist via Windows service". Static analysis is limited against packed or obfuscated samples; in those cases unpack the file first, or run it in a supported sandbox and submit the resulting report instead.
