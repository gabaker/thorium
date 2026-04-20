# YARA

YARA is a tool aimed at (but not limited to) helping malware researchers identify and classify malware samples. With YARA you can create descriptions of malware families (or anything else) based on textual or binary patterns.

## Overview

Each YARA description, called a rule, consists of a set of strings and a boolean expression that determines its logic. Rules support wildcards, case-insensitive strings, regular expressions, and special operators to match string patterns and byte sequences within a file. In this Thorium deployment, YARA scans each sample against a precompiled set of open-source rule collections (including CAPEv2, Yara-Rules, signature-base, ReversingLabs, CDI, and NCC Group rules) and reports matching rule names, tags, and metadata. Note: the upstream YARA project is in maintenance mode, with YARA-X as its successor.

## Supported File Types

- Any file / arbitrary binary data (YARA matches textual and binary patterns against the raw bytes of a file)

## Usage

Run YARA when you want to classify a sample or detect known malware families and indicators of compromise via signature matching. It is well suited to malware classification, threat hunting, and incident response, and is most useful when a file may match documented patterns from public rule sets. Matching rule names are surfaced as `YaraRuleHits` tags so samples can be triaged or pivoted on by detected rule.

## Documentation

[VirusTotal/yara on GitHub](https://github.com/VirusTotal/yara)

## License

BSD-3-Clause — see the project's [COPYING](https://github.com/VirusTotal/yara/blob/master/COPYING).
