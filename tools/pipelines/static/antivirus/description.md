# Antivirus

Scans submitted samples for known malware using the ClamAV open source antivirus engine, flagging files that match its regularly updated signature database.

## Images Run

- **clamav** — Runs `clamscan` to match the sample against ClamAV's signature database, reporting the detected signature name on a hit and "Ok" when no known malware is found.

## Supported File Types

Any file / arbitrary binary data — ClamAV scans whatever sample it is given. Archive and installer formats (for example 7z/lzma, NSIS/NulSoft, ZIP, and RAR when runtime UnRAR support is available) are unpacked and scanned internally.

## Usage

Run this pipeline as a baseline antivirus check during file triage to quickly flag samples that match known malware signatures before deeper manual analysis. It is broadly applicable to any submitted file and is most valuable for identifying already-known malicious binaries, archives, and installers across many malware families. A match returns the signature name as a result and tag; no match yields an "Ok" result.
