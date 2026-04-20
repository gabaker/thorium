# Detect It Easy (DiE)

Detect It Easy (DiE) is a tool for file type identification, widely used by malware analysts, cybersecurity experts, and reverse engineers. It supports both signature-based and heuristic analysis.

## Overview

DiE identifies file formats and detects the compilers, packers, protectors, and other tooling used to build or transform a file. Its detection architecture is script-driven, using a JavaScript-like scripting language (DiE-JS, an ES5 runtime) for custom detection rules, and combines signature and heuristic scanning to reduce false positives. Unknown or unrecognized formats undergo heuristic analysis so that both known and unknown files can be characterized. In this image the tool runs via its command-line scanner (`diec`) and emits JSON results.

## Supported File Types

Detect It Easy supports a wide range of executable and archive types, including:

- PE (Windows Portable Executable)
- ELF (Linux Executable and Linkable Format)
- MACH (Mach-O for macOS)
- MS-DOS and COM executables
- LE/LX (Linear Executable for OS/2)
- Amiga executables
- APK (Android Application Package)
- IPA (iOS Application Package)
- DEX (Dalvik Executable)
- JAR (Java Archive)
- ZIP archives
- ISO9660 (optical media)
- NPM (JavaScript packages)
- Binary / other unclassified files

The list expands as the tool is updated, and unknown formats are still processed through heuristic analysis.

## Usage

Run DiE on any executable, archive, or unknown file to identify its format and to detect the compiler, packer, or protector used to produce it. It is especially useful for malware triage and static analysis: quickly determining how a binary was built or packed helps guide later steps such as unpacking, disassembly, or security review. It is also applicable to software forensics and security audits where understanding a file's components and origin matters.

## Documentation

[Detect It Easy on GitHub](https://github.com/horsicq/Detect-It-Easy)

## License

MIT — see the project's [LICENSE](https://github.com/horsicq/Detect-It-Easy/blob/master/LICENSE).
