# Binwalk

Binwalk (v3) is a firmware analysis and file-carving tool, re-written in Rust for speed and accuracy. It identifies and optionally extracts files and data that have been embedded inside of other files.

## Overview

Binwalk's primary focus is firmware analysis, but it supports a wide variety of file and data types via a comprehensive set of signatures. It can identify and extract embedded files, and through entropy analysis it can help identify unknown compression or encryption. In this Thorium deployment it runs in extraction mode, carving any embedded files it recovers as child samples for further analysis.

## Supported File Types

- Firmware images
- Any binary containing embedded files or data matching Binwalk's signatures (a wide variety of file and data types is supported; see the Supported Signatures wiki)

## Usage

Run Binwalk on any sample suspected of containing embedded files or data, such as firmware images, disk images, archives, or other binaries with embedded content. It is typically used as an early step in firmware reverse engineering to extract filesystem contents, bootloaders, and other embedded components, and its entropy analysis can flag regions of unknown compression or encryption. It will not recover content from samples that are themselves encrypted.

## Documentation

[ReFirmLabs/binwalk on GitHub](https://github.com/ReFirmLabs/binwalk)

## License

MIT License (per the project repository). The authoritative README does not itself name a license.
