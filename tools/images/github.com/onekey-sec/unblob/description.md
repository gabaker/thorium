# unblob

unblob is an accurate, fast, and easy-to-use extraction suite for binary blobs. It parses unknown binary blobs for 78+ archive, compression, and file-system formats, extracts their content recursively, and carves out unknown chunks.

## Overview

unblob detects each chunk in a blob using precise start and end offsets according to the format standard, which minimizes false positives, and recursively extracts containers within containers up to a configurable depth (default 10). Data that does not match any known format is carved out as unknown chunks, with automatic identification of null/`0xFF` padding and entropy analysis (Shannon entropy and chi-square probability) to help spot encrypted or compressed regions. It produces structured JSON metadata reports covering chunk offsets, sizes, entropy, file ownership, permissions, and timestamps, uses multi-processing across CPU cores for speed, and supports custom format handlers via an extensible plugin system. It runs as a regular user without elevated privileges.

## Supported File Types

- Archives (e.g. ZIP, 7-Zip, CPIO, tar)
- Compression streams (e.g. gzip, XZ, LZMA, LZ4)
- File systems (e.g. SquashFS, JFFS2, UBI/UBIFS, ext)
- 78+ formats in total; arbitrary binary blobs are scanned, with unknown regions carved out

## Usage

Run unblob on firmware images, disk images, or any binary blob suspected of containing embedded or nested content. It is especially effective for firmware analysis and reverse engineering workflows where multiple nested layers of archives, compression streams, and file systems must be peeled apart recursively. It is also useful when you need to locate and extract embedded data from an otherwise unknown file, since it carves and reports unknown chunks and flags high-entropy (potentially encrypted or compressed) regions.

## Documentation

[unblob documentation](https://unblob.org)

## License

MIT License
