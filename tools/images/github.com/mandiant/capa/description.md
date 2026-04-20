# capa

capa detects capabilities in executable files. You run it against a PE, ELF, .NET module, shellcode file, or a sandbox report and it tells you what it thinks the program can do (for example, that the file is a backdoor, can install services, or relies on HTTP to communicate).

## Overview

capa uses a collection of community-contributed rules to identify behaviors and capabilities in programs. Detected capabilities are mapped to the MITRE ATT&CK framework and the Malware Behavior Catalog (MBC), giving both high-level tactic/technique classifications and fine-grained capability matches such as "connect to HTTP server", "encode data using XOR", or "persist via Windows service". This deployment runs capa's static analysis against the submitted file using the bundled capa rules and signatures, and also flags samples that appear to be packed.

## Supported File Types

- PE executables
- ELF executables
- .NET modules
- Shellcode files
- Sandbox analysis reports (e.g. CAPE, DRAKVUF, VMRay) for dynamic capability detection

## Usage

Run capa for initial triage of suspicious or unknown executables to quickly determine what a program is capable of without manual reverse engineering. It is well suited to PE, ELF, .NET, and shellcode samples, and reports ATT&CK tactics/techniques alongside detailed capability matches. Static analysis is limited against packed or obfuscated samples; in those cases unpack the file first, or run it in a supported sandbox and analyze the resulting report instead.

## Documentation

[mandiant/capa on GitHub](https://github.com/mandiant/capa)

## License

Apache-2.0
