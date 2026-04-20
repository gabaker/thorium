# unblob

A single-stage static extraction pipeline that parses unknown binary blobs for 78+ archive, compression, and file-system formats and recursively extracts their content. It carves out unknown chunks and produces structured JSON metadata reports describing each detected region.

## Images Run

- **unblob** — Accurate, fast extraction suite that detects chunks by precise format-defined start/end offsets, recursively extracts containers within containers (default depth 10), carves unknown chunks, and reports entropy (Shannon and chi-square) plus JSON metadata on offsets, sizes, ownership, permissions, and timestamps.

## Supported File Types

Arbitrary binary blobs are scanned, with unknown regions carved out. Across 78+ supported formats this includes:

- Archives (e.g. ZIP, 7-Zip, CPIO, tar)
- Compression streams (e.g. gzip, XZ, LZMA, LZ4)
- File systems (e.g. SquashFS, JFFS2, UBI/UBIFS, ext)

## Usage

Run this pipeline on firmware images, disk images, or any binary blob suspected of containing embedded or nested content. It is especially effective for firmware analysis and reverse engineering workflows where multiple nested layers of archives, compression streams, and file systems must be peeled apart recursively. It is also useful for locating and extracting embedded data from an otherwise unknown file, since it carves and reports unknown chunks and flags high-entropy (potentially encrypted or compressed) regions.
