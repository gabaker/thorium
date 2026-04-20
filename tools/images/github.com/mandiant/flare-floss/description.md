# FLARE Obfuscated String Solver (FLOSS)

The FLARE Obfuscated String Solver (FLOSS, formerly FireEye Labs Obfuscated String Solver) uses advanced static analysis techniques to automatically extract and deobfuscate strings from malware binaries. It can be used much like the `strings` utility to enhance basic static analysis of unknown binaries.

## Overview

Malware authors often evade heuristic detection by obfuscating only key portions of an executable, such as the strings and resources used to configure domains, files, and other infection artifacts that do not appear as plaintext in standard `strings` output. FLOSS extracts four string types: static strings (regular ASCII and UTF-16LE), stack strings (constructed on the stack at run-time), tight strings (a special form of stack strings decoded on the stack), and decoded strings (decoded within a function). It additionally supports language-specific string extraction for binaries compiled from Go and Rust.

## Supported File Types

- Executable binaries (the kind analyzed during static malware analysis, used like `strings.exe`)
- Programs compiled from Go and Rust (via language-specific string extraction)

## Usage

Run FLOSS on executable binaries, especially suspected or known malware samples, to recover obfuscated strings that the standard `strings` utility would miss. It is particularly effective at surfacing C2 domains, file paths, registry keys, and configuration values that authors have hidden using XOR, stack construction, or other in-function decoding routines. It is also useful for Go and Rust binaries, whose string formats the classic `strings` algorithm does not handle well.

## Documentation

[mandiant/flare-floss on GitHub](https://github.com/mandiant/flare-floss)

## License

Apache-2.0
