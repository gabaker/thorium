# UEFI Firmware Parser

UEFI Firmware Parser is a Python module and set of scripts for parsing, extracting, and recreating UEFI firmware volumes, including parsing support for BIOS, OptionROM, Intel ME, and related formats.

## Overview

The tool's `AutoParser` automatically detects the firmware type and walks the structure, printing a hierarchy of firmware volumes, files, and sections. In this Thorium deployment, a wrapper runs the parser with brute-force search enabled, writes the parsed object hierarchy to a JSON structure file, and extracts embedded executable sections (PE32, PIC, TE) as child files named by their UI label or GUID. Beyond UEFI volumes, the upstream module also handles Intel PCH flash descriptors, Intel ME/TXE modules, Dell PFS updates, and Tiano/EFI plus native LZMA (de)compression.

## Supported File Types

- UEFI firmware volumes, capsules, filesystems, files, and sections
- BIOS / UEFI firmware images and dumps
- OptionROM images
- Intel ME / TXE modules
- Intel PCH flash descriptors
- Dell PFS (HDR) updates
- Arbitrary binary blobs that may contain firmware objects (detected via brute-force/byte-by-byte search)

## Usage

Run this tool on firmware images, BIOS dumps, or binary blobs suspected of containing UEFI firmware volumes or related structures (Intel ME, OptionROM, Dell PFS). It is most useful for firmware security analysis, supply-chain verification, and reverse engineering, where you need to recover the nested firmware object hierarchy and pull out the individual executable components for follow-on analysis. Because it brute-force searches, it is worth running even when the firmware type is not obvious from the file's surface metadata.

## Documentation

[theopolis/uefi-firmware-parser on GitHub](https://github.com/theopolis/uefi-firmware-parser)

## License

MIT (per the upstream project and the bundled source file copyright headers; the fetched documentation does not name a license explicitly).
