# StringSifter

StringSifter is a machine learning tool from Mandiant that automatically ranks strings based on their relevance for malware analysis.

## Overview

StringSifter uses a supervised learning-to-rank model (Gradient Boosted Decision Trees trained on strings from sampled malware binaries) to surface the most analytically relevant strings first, instead of leaving an analyst to read through thousands of extracted strings. It ships two scripts: `flarestrings`, which mimics features of GNU Binutils' `strings`, and `rank_strings`, which accepts piped string output and ranks it by relevance. In this Thorium image, a submitted file is run through `flarestrings` and piped into `rank_strings`, producing a single relevance-ordered list of strings.

## Supported File Types

- Any file / arbitrary binary data (printable strings are extracted from the raw bytes)

StringSifter can also be applied to arbitrary lists of pre-extracted strings, including output from tools such as `strings`, FLOSS, sandbox runs, or memory dumps.

## Usage

Run StringSifter during initial triage of a binary or malware sample to quickly identify the most interesting strings, such as C2 domains, API calls, file paths, and configuration values, without manually reading every extracted string. It is most useful when a sample yields a large volume of strings and you want a prioritized, relevance-ranked view to speed up analysis. Because it works on any file, it is a good default static-analysis step for unknown or suspicious binaries.

## Documentation

[StringSifter on GitHub](https://github.com/mandiant/stringsifter)

## License

Apache-2.0 — see the project's [LICENSE](https://github.com/mandiant/stringsifter/blob/master/LICENSE).
