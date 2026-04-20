# Balbuzard

Balbuzard is a package of Python malware-analysis tools that extract patterns of interest from suspicious files (IP addresses, domain names, known file headers, interesting strings, etc.). This image runs the `balbuzard` extraction tool against the submitted file.

## Overview

Written by Philippe Lagadec, `balbuzard` scans a file for patterns of interest such as IP addresses, URLs, embedded files, and typical malware strings, and is easily extensible with new patterns, regular expressions, and YARA rules. The broader Balbuzard package also ships companion tools not invoked by this image: `bbcrack` (bruteforces typical malware obfuscation such as XOR, ROL, ADD and combinations to guess algorithms/keys), `bbharvest` (extracts all patterns found across all possible obfuscation transforms and keys), and `bbtrans` (applies those transforms to a file).

## Supported File Types

- Any file / arbitrary binary data (suspicious files, executables, documents, etc.)

## Usage

Run `balbuzard` as an early triage step on a new or suspicious file to extract strings and patterns of interest (IPs, domains, URLs, file headers, malware strings) and to detect embedded files in cleartext. It is most useful when you want a quick inventory of indicators before deeper analysis; if the file appears to hide data behind obfuscation such as XOR, the companion `bbcrack`/`bbharvest` tools (not run by this image) are the recommended follow-up.

## Documentation

[Balbuzard on GitHub](https://github.com/decalage2/balbuzard)

## License

BSD-2-Clause. Copyright (c) 2007-2019, Philippe Lagadec. The license applies to the whole Balbuzard package (balbuzard, bbcrack, bbharvest, bbtrans) except the thirdparty and plugins folders, which contain third-party files under their own licenses.
