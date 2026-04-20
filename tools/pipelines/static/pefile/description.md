# pefile

Parses Windows Portable Executable (PE) files to extract detailed structural information — headers, sections, imports, exports, resources, and embedded data — as structured JSON, along with an import hash tag and parsing warnings.

## Images Run

- **pefile** — Multi-platform Python module that loads the PE file, writes its full `dump_dict()` output as JSON, surfaces parsing warnings, and records the import hash (imphash) as a tag.

## Supported File Types

Windows Portable Executable (PE) files (e.g. .exe, .dll, .sys). The pipeline is triggered automatically on samples tagged with the `exe` or `dll` file type extension.

## Usage

Run this pipeline on Windows PE binaries when you need structured, detailed access to the PE format: headers, sections, imports/exports, resources, and embedded data. It is a foundational step in Windows malware and binary analysis workflows, and the emitted imphash tag and parsing warnings are useful for clustering related samples and flagging malformed or suspicious files.
