# YARA

Scans submitted samples against a precompiled set of open-source YARA rule collections to identify and classify malware based on textual and binary patterns.

## Images Run

- **yara** — Runs the YARA pattern-matching scanner against the sample, reporting matching rule names, tags, and metadata as `YaraRuleHits` tags.

## Supported File Types

Any file / arbitrary binary data. YARA matches textual and binary patterns against the raw bytes of a file, so it has no format restrictions.

## Usage

Run this pipeline when you want to classify a sample or detect known malware families and indicators of compromise via signature matching. It scans each sample against bundled open-source rule sets (including CAPEv2, Yara-Rules, signature-base, ReversingLabs, CDI, and NCC Group rules) and is well suited to malware classification, threat hunting, and incident response. It is most useful when a file may match documented patterns from public rule collections, and matching rule names are surfaced as `YaraRuleHits` tags for triage and pivoting.
