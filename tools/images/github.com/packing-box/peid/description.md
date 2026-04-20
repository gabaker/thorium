# PEiD (Packed Executable iDentifier)

PEiD is a Python implementation of the Packed Executable iDentifier, a tool that detects packers on Windows PE files based on signatures.

## Overview

PEiD checks an input executable against a database of more than 5,500 signatures merged from several sources (wolfram77web/app-peid, merces/pev, ExeinfoASL/ASL, Ice3man543/MalScan, and PEiD Tab) to identify the packer used on the file. It supports both the embedded signature database and user-defined databases. The project also ships companion utilities to inspect signatures (`peid-db`) and to create and integrate new signatures (`peid-sig`), though this image runs only the main `peid` scan.

## Supported File Types

- Windows PE executables (Portable Executable format)

## Usage

Run PEiD on Windows PE executables to determine whether the file has been packed and, if so, which packer was used. Identifying the packer is a useful early step in static malware analysis because it indicates whether the sample must be unpacked before further analysis. This tool is not applicable to non-PE files; in this image, inputs lacking a valid DOS header are reported as such rather than analyzed.

## Documentation

[packing-box/peid (GitHub)](https://github.com/packing-box/peid)

## License

GPL-3.0 — see the project's [LICENSE](https://github.com/packing-box/peid/blob/main/LICENSE).
