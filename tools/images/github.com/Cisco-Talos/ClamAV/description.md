# ClamAV

ClamAV is an open source antivirus engine, developed by Cisco Talos, for detecting trojans, viruses, malware, and other malicious threats.

## Overview

ClamAV provides a command-line scanner (`clamscan`) that matches files against a regularly updated signature database to identify known malware. In this image it runs `clamscan` over a submitted sample and reports the detected signature name when a match is found. ClamAV includes parsers for archive and installer formats (for example NSIS/NulSoft, 7z/lzma, and ZIP), and RAR support is loaded at runtime when available. Analysts can also author custom ClamAV signatures for targeted detection.

## Supported File Types

- Any file / arbitrary binary data (ClamAV scans whatever sample it is given)
- Archive and installer formats are unpacked and scanned internally (e.g. 7z/lzma, NSIS/NulSoft, and RAR when runtime UnRAR support is available)

## Usage

Run ClamAV as a baseline antivirus check during file triage to quickly flag samples that match known malware signatures before deeper manual analysis. It is broadly applicable to any submitted file and is most valuable for identifying already-known malicious binaries, archives, and installers across many malware families. A match returns the signature name as a result and tag; no match yields an "Ok" result.

## Documentation

[ClamAV Documentation](https://docs.clamav.net/)

## License

GNU General Public License, Version 2 (GPL-2.0). Note that some bundled third-party components are licensed differently (for example Yara under Apache 2.0, and the runtime-loaded UnRAR under a non-free/restricted license).
