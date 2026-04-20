# Binwalk

Identifies and extracts files and data embedded inside other files using the Binwalk file-carving tool, carving any recovered content as child samples for further analysis.

## Images Run

- **binwalk** — Binwalk (v3), a firmware analysis and file-carving tool re-written in Rust; runs in extraction mode to identify and carve embedded files and data from the submitted sample.

## Supported File Types

- Firmware images
- Any binary containing embedded files or data that matches Binwalk's signatures (a wide variety of file and data types is supported; see the Supported Signatures wiki). Common targets include firmware images, disk images, and archives. It will not recover content from samples that are themselves encrypted.

## Usage

Run this pipeline on any sample suspected of containing embedded files or data, such as firmware images, disk images, archives, or other binaries with embedded content. It is typically used as an early step in firmware reverse engineering to extract filesystem contents, bootloaders, and other embedded components, and its entropy analysis can flag regions of unknown compression or encryption. Carved files are emitted as child samples for downstream analysis. Encrypted samples cannot be carved.
