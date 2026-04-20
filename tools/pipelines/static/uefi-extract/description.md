# UEFI Extract

Parses UEFI firmware images and related formats, recovering the nested firmware object hierarchy and extracting embedded executable components for follow-on analysis.

## Images Run

- **uefi-firmware-parser** — Parses, extracts, and recreates UEFI firmware volumes (and BIOS, OptionROM, Intel ME/TXE, Intel PCH flash descriptors, and Dell PFS formats), writes the parsed object hierarchy to a JSON structure file, and extracts embedded executable sections (PE32, PIC, TE) as named child files. Runs with brute-force search enabled so it can locate firmware structures even inside arbitrary binary blobs.

## Supported File Types

- UEFI firmware volumes, capsules, filesystems, files, and sections
- BIOS / UEFI firmware images and dumps
- OptionROM images
- Intel ME / TXE modules
- Intel PCH flash descriptors
- Dell PFS (HDR) updates
- Arbitrary binary blobs that may contain firmware objects (detected via brute-force/byte-by-byte search)

## Usage

Run this pipeline on firmware images, BIOS dumps, or binary blobs suspected of containing UEFI firmware volumes or related structures (Intel ME, OptionROM, Dell PFS). It recovers the nested firmware object hierarchy and pulls out individual executable components (PE32, PIC, TE) as child files for further analysis, making it well suited to firmware security analysis, supply-chain verification, and embedded system reverse engineering. Because it brute-force searches, it is worth running even when the firmware type is not obvious from the file's surface metadata.
