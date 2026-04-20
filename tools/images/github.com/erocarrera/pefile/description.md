# pefile

pefile is a multi-platform Python module that parses and works with Portable Executable (PE) files. Most of the information contained in the PE file headers is accessible, as well as all of the sections' details and data.

## Overview

pefile exposes the structures defined in the Windows header files as attributes on a PE instance, largely following the naming scheme of those headers. It can inspect headers, analyze section data, retrieve embedded data, read strings from resources, emit warnings for suspicious or malformed values, and detect packers using PEiD signatures. In this Thorium image, the tool loads the input file with pefile and writes the full `dump_dict()` output as JSON, surfaces any parsing warnings, and records the import hash (imphash) as a tag.

## Supported File Types

- Windows Portable Executable (PE) files (e.g. .exe, .dll, .sys)

## Usage

Run pefile on Windows PE binaries when you need structured, detailed access to the PE format: headers, sections, imports/exports, resources, and embedded data. It is a foundational step in Windows malware and binary analysis workflows, and the emitted imphash tag and parsing warnings are useful for clustering related samples and flagging malformed or suspicious files.

## Documentation

[erocarrera/pefile on GitHub](https://github.com/erocarrera/pefile)

## License

MIT — see the project's [LICENSE](https://github.com/erocarrera/pefile/blob/master/LICENSE).
