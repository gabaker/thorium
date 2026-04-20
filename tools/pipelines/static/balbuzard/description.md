# Balbuzard

Runs the `balbuzard` extraction tool against a submitted file to surface patterns of interest such as IP addresses, domain names, URLs, embedded file headers, and typical malware strings.

## Images Run

- **balbuzard** — Python-based extraction tool that scans a file for patterns of interest (IP addresses, URLs, embedded files, known file headers, typical malware strings); extensible with custom patterns, regular expressions, and YARA rules.

## Supported File Types

Any file / arbitrary binary data. Balbuzard operates on suspicious files of any kind, including executables, documents, and other binary artifacts.

## Usage

Run this pipeline as an early triage step on a new or suspicious file to extract strings and indicators of interest (IPs, domains, URLs, file headers, malware strings) and to detect embedded files in cleartext. It is most useful when you want a quick inventory of indicators before deeper analysis. If the file appears to hide data behind obfuscation such as XOR, the broader Balbuzard package's companion tools (bbcrack/bbharvest, not invoked by this pipeline) are the recommended follow-up.
