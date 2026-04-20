# UPX

UPX (the Ultimate Packer for eXecutables) is a free, portable, high-performance executable packer for several executable formats. In this toolbox it is run in decompression (unpack) mode (`upx -d`) to restore UPX-packed binaries to their original, unpacked form.

## Overview

UPX is an advanced executable file compressor that typically reduces the size of programs and DLLs by around 50-70% while keeping them self-contained and runnable with no runtime or memory penalty for most supported formats. It supports a number of different executable formats, including Windows programs and DLLs and Linux executables. In this deployment the wrapper invokes UPX's decompression mode to unpack a previously UPX-packed sample, writing the restored binary as a child artifact.

## Supported File Types

UPX-packed executables, including:

- Windows programs and DLLs
- Linux executables

## Usage

Run this tool on samples that have been identified as UPX-packed (for example by a packer-detection tool such as Detect-It-Easy). Unpacking reverses the UPX compression and recovers the original executable, which then enables more effective static analysis with tools like strings, disassemblers, and signature scanners. Files that are not UPX-packed cannot be unpacked by this tool.

## Documentation

[UPX home page](https://upx.github.io)

## License

Distributed with full source code under the GNU General Public License v2 or later (GPL-2.0-or-later): either the pure GPLv2+, or, at the user's option, the GPLv2+ with special exceptions granting free usage for all binaries including commercial programs.
