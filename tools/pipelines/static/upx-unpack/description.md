# upx-unpack

Decompresses UPX-packed executables back to their original, unpacked form so the recovered binary can be examined with further static analysis tooling.

## Images Run

- **upx-unpack** — Runs UPX (the Ultimate Packer for eXecutables) in decompression mode (`upx -d`) to unpack a UPX-packed sample, writing the restored binary as a child artifact.

## Supported File Types

UPX-packed executables, including Windows programs and DLLs and Linux executables. Files that are not UPX-packed cannot be unpacked by this pipeline.

## Usage

Run this pipeline on samples identified as UPX-packed (for example, files tagged with a UPX detection by a packer-detection tool such as Detect-It-Easy). Unpacking reverses the UPX compression and recovers the original executable, which then enables more effective static analysis with tools like strings, disassemblers, and signature scanners. UPX packing is commonly used by malware to compress and obfuscate executables, so this is typically run before deeper static analysis of suspected packed binaries.
