# Pharos ApiAnalyzer

ApiAnalyzer is a tool from the CMU SEI Pharos static binary analysis framework for finding sequences of API calls with specified data and control relationships in binary programs.

## Overview

ApiAnalyzer is part of the Pharos framework developed by the Software Engineering Institute at Carnegie Mellon University, built on the ROSE compiler infrastructure for disassembly, control flow analysis, and instruction semantics. It is designed to detect common operating system interaction paradigms, such as opening a file, writing to it, and then closing it, by identifying API call sequences that match specified patterns. In this deployment it runs in batch mode against both 32-bit and 64-bit executables and emits results in JSON format.

## Supported File Types

- Compiled executable binaries (PE executables), 32-bit and 64-bit

## Usage

Run ApiAnalyzer on executable binaries to identify sequences of API calls that reveal higher-level program behavior, such as file operations and other operating system interaction paradigms, without manual reverse engineering. It is most useful when triaging or characterizing a suspect binary and you want to surface meaningful API call patterns rather than isolated imports.

## Documentation

[cmu-sei/pharos](https://github.com/cmu-sei/pharos)

## License

BSD-3-Clause. Copyright Carnegie Mellon University.
