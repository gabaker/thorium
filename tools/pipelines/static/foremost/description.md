# Foremost

Recovers files from disk images, raw dumps, and other binary data by carving out regions that match known file header and footer signatures, returning the recovered files as child artifacts along with an audit report.

## Images Run

- **foremost** — File recovery and carving tool (originally developed by the US Air Force OSI) that scans raw data for header/footer byte signatures and extracts matching regions as recovered files; runs in verbose mode (`foremost -V`) over the input.

## Supported File Types

- Disk and forensic image files (e.g. dd, Safeback, Encase) or raw drive data
- Any binary input that may contain embedded files matching the configured header/footer signatures

## Usage

Run this pipeline when you have a disk image, raw dump, or other binary blob that may contain embedded or deleted files you want to extract. It is well suited to digital-forensics workflows where files must be recovered or carved out of unstructured data using known file signatures rather than filesystem metadata. Carved outputs are returned as children for further analysis, and an audit report of what was recovered is captured as the result.
