# ssdeep

ssdeep computes context triggered piecewise hashes (CTPH), also known as fuzzy hashes, which can be used to identify files that are similar but not identical.

## Overview

ssdeep generates a fuzzy hash signature for a file and can compare two such signatures to produce a match score from 0 (no match) to 100. Unlike a fixed comparison of cryptographic hashes, CTPH is designed to detect files that share content even after insertions, deletions, or modifications. In this deployment the wrapper runs `ssdeep` on the submitted file and records the resulting fuzzy hash signature as a string result.

## Supported File Types

- Any file / arbitrary binary data (ssdeep hashes the raw bytes of a file regardless of format)

## Usage

Run ssdeep on a sample to obtain its fuzzy hash so it can be compared against other samples for similarity. This is useful for finding near-identical or partially overlapping files, such as variants of the same artifact or modified versions of a known file, where exact-match cryptographic hashes would not reveal the relationship.

## Documentation

[ssdeep project repository](https://github.com/ssdeep-project/ssdeep)

## License

GPL-2.0 — see the project's [COPYING](https://github.com/ssdeep-project/ssdeep/blob/master/COPYING).
