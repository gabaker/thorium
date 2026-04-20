# Pharos

A comprehensive static binary analysis pipeline built on the CMU SEI Pharos framework (ROSE compiler infrastructure), running four specialized analyzers against executable binaries to surface API behavior, call parameters, function fingerprints, and recovered C++ class structure.

## Images Run

- **pharos-apianalyzer** — Detects sequences of API calls with specified data and control relationships, revealing OS interaction paradigms such as file operations and network communication.
- **pharos-callanalyzer** — Reports the resolved static parameter values passed to API and library calls, exposing concrete values like file paths and registry keys without executing the program.
- **pharos-fn2hash** — Computes per-function hashes and descriptive properties to support binary similarity analysis, code-reuse detection, and machine-learning features.
- **pharos-ooanalyzer** — Recovers object-oriented constructs (C++ classes, members, and methods) from compiled executables using Prolog-based logic programming; currently limited to 32-bit x86 MSVC binaries.

## Supported File Types

Compiled executable binaries (PE executables). ApiAnalyzer, CallAnalyzer, and FN2Hash handle both 32-bit and 64-bit binaries, while OOAnalyzer only supports 32-bit x86 executables compiled with Microsoft Visual C++ (MSVC).

## Usage

Submit PE executables to this pipeline for in-depth static binary analysis that goes beyond simple imports and whole-file hashing. Use it when triaging or reverse engineering a suspect binary and you want API call patterns, the concrete parameters flowing into system calls, and function-level fingerprints for similarity clustering. For C++ class recovery via OOAnalyzer, the input must be a confirmed 32-bit MSVC binary, as other architectures and compilers are not supported by that stage.
