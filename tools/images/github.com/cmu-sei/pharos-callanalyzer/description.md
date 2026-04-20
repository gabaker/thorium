# Pharos CallAnalyzer

CallAnalyzer is a tool from the CMU SEI Pharos static binary analysis framework for reporting the static parameters passed to API calls in a binary program.

## Overview

CallAnalyzer is part of the Pharos framework developed by the Software Engineering Institute at Carnegie Mellon University, built on the ROSE compiler infrastructure for disassembly, control flow analysis, and instruction semantics. It demonstrates the framework's calling convention, parameter analysis, and type detection capabilities while also providing useful analysis of the code in a program. For each API call it identifies, it reports the resolved static parameter values. In this image it runs in batch mode and emits results as pretty-printed JSON.

## Supported File Types

- Executable / binary programs analyzable by the Pharos framework
- Both 32-bit and 64-bit binaries (the image is configured with `--allow-64bit`)

## Usage

Run CallAnalyzer on executable binaries to extract the concrete parameter values passed to API and library calls without executing the program. This surfaces values such as file paths, registry keys, and other arguments handed to system functions, helping analysts understand a program's intended behavior through static analysis. It is most useful during reverse engineering and triage when you need to recover the data flowing into API calls.

## Documentation

[Pharos Static Binary Analysis Framework](https://github.com/cmu-sei/pharos)

## License

BSD license. Copyright Carnegie Mellon University.
